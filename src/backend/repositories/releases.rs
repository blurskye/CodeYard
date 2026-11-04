//! Releases: a named, optionally annotated tag with notes and uploaded assets. Readers see the
//! published releases of a repository they can read; writers also see drafts and make changes.
//! A tag that does not exist yet is created when the release is published, so a draft never
//! publishes its tag.
pub(crate) mod immutable;
pub(crate) mod integrity;
mod notes;
use crate::backend::ci::CiRuns;
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::files;
use crate::backend::files::DownloadError;
use crate::backend::files::UploadDeclaration;
use crate::backend::files::UploadError;
use crate::backend::notifications::watchers::notify_watchers;
use crate::backend::notifications::watchers::WatchEvent;
use crate::backend::notifications::watchers::WatchedItem;
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::access;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::refs;
use crate::backend::repositories::refs::RefError;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::RepoError;
use crate::backend::storage::ObjectStore;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use anyhow::Error;
use codeyard_db_core::models::Release;
use codeyard_db_core::models::ReleaseAsset;
use codeyard_db_core::models::ReleaseRow;
use codeyard_db_core::models::Repository;
use codeyard_git as git;
pub use notes::{generate_notes, NotesInput, ReleaseNotes};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

pub use crate::backend::db::releases::{
    LatestSelection, ReleaseChanges, ReleaseFields, ReleaseRefusal,
};

pub const MAX_ASSET_BYTES: usize = crate::backend::storage::MAX_OBJECT_BYTES as usize;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authority_storage_failure_is_not_a_permission_refusal() {
        let raw = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let pool = codeyard_db_core::DbPool::Sqlite(raw);
        pool.close().await;
        assert!(matches!(
            can_write(&pool, Some(1), 1).await,
            Err(ReleaseError::Access(AccessError::Unavailable))
        ));
        assert!(!can_write(&pool, None, 1).await.unwrap());
    }
}

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
            RefError::Protected(message) if message == immutable::TAG_PROTECTED => {
                Self::Invalid(message)
            }
            error => Self::Ref(error),
        }
    }
}

/// What a writer sends to publish a release. The tag and the target only matter when the tag
/// does not exist yet: then it is created at `target`, a branch or commit.
#[derive(Debug, Clone, Default)]
pub struct ReleaseInput {
    pub repository_id: i64,
    pub tag: String,
    pub target: String,
    pub fields: ReleaseFields,
    /// Set when the caller may not create tags, as a token without the workflow grant may not.
    /// A release that needs a new tag is then refused where the tag would be created.
    pub new_tag_denied: bool,
}

pub struct ReleasesPage {
    pub repository: Repository,
    pub can_write: bool,
    pub releases: Vec<ReleaseRow>,
    pub assets: Vec<ReleaseAsset>,
    pub total: i64,
}

pub struct ReleaseDetail {
    pub repository: Repository,
    pub can_write: bool,
    pub release: ReleaseRow,
    pub assets: Vec<ReleaseAsset>,
}

/// What the release form offers: the tags it can publish and the branches a new tag can start on.
pub struct FormChoices {
    pub repository: Repository,
    pub tags: Vec<String>,
    pub branches: Vec<String>,
    pub default_branch: String,
}

/// The repository's latest published release and how many releases the viewer can see.
pub struct ReleaseSummary {
    pub count: i64,
    pub latest: Option<Release>,
}

async fn writable(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    owner: &str,
    name: &str,
) -> Result<(Repository, codeyard_db_core::models::User), ReleaseError> {
    let repository = db::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| ReleaseError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    Ok(access::check_write(pool, repository.id, actor).await?)
}

async fn can_write(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repo_id: i64,
) -> Result<bool, ReleaseError> {
    match viewer {
        Some(actor) => {
            let repo = access::current_repository(pool, repo_id).await?;
            let Some(user) = db::users::accounts::get_user_by_id(pool, actor)
                .await
                .map_err(unavailable)?
            else {
                return Ok(false);
            };
            Ok(access::user_access(pool, &user, &repo)
                .await
                .map_err(unavailable)?
                .can_write())
        }
        None => Ok(false),
    }
}

fn unavailable<E>(_: E) -> ReleaseError {
    ReleaseError::Unavailable
}

/// Maps a refusal the database raised inside a write to the error the caller shows.
fn write_error(error: Error) -> ReleaseError {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return ReleaseError::Access(AccessError::WriteDenied);
    }
    match error.downcast_ref::<ReleaseRefusal>() {
        Some(ReleaseRefusal::Denied) => ReleaseError::Access(AccessError::WriteDenied),
        Some(ReleaseRefusal::NotFound) => ReleaseError::NotFound,
        Some(ReleaseRefusal::TagTaken) => ReleaseError::TagTaken,
        Some(ReleaseRefusal::AssetNameTaken) => ReleaseError::AssetNameTaken,
        Some(ReleaseRefusal::Immutable) => {
            ReleaseError::Invalid("An immutable release cannot change assets or return to a draft")
        }
        Some(ReleaseRefusal::TagReserved) => {
            ReleaseError::Invalid("The tag name is reserved by an immutable release")
        }
        Some(ReleaseRefusal::Conflict) => ReleaseError::Conflict,
        Some(ReleaseRefusal::TooManyAssets) => {
            ReleaseError::Invalid("A release can contain at most 1000 assets")
        }
        None => ReleaseError::Unavailable,
    }
}

fn mutation_error(error: RepoError) -> ReleaseError {
    if error.is_write_denied() {
        return ReleaseError::Access(AccessError::WriteDenied);
    }
    if error.kind == codeyard_git::error::ErrorKind::NotFound {
        return ReleaseError::NotFound;
    }
    if error.kind == codeyard_git::error::ErrorKind::Conflict
        && error.message == ReleaseRefusal::Conflict.to_string()
    {
        return ReleaseError::Conflict;
    }
    if error.kind == codeyard_git::error::ErrorKind::Invalid
        && error.message == ReleaseRefusal::TagTaken.to_string()
    {
        return ReleaseError::TagTaken;
    }
    ReleaseError::Git(error)
}

fn validate_fields(fields: &ReleaseFields) -> Result<(), ReleaseError> {
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

fn validate_tag(tag: &str) -> Result<(), ReleaseError> {
    if tag.is_empty() || tag.len() > 255 || git::tag_ref(tag).is_none() {
        return Err(ReleaseError::Invalid("Tag name is not valid"));
    }
    Ok(())
}

/// The release list, newest first. Drafts are listed only for writers.
pub async fn list(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repo_id: i64,
    page: i64,
    per_page: i64,
) -> Result<ReleasesPage, ReleaseError> {
    let offset = page
        .checked_sub(1)
        .filter(|_| page > 0 && (1..=100).contains(&per_page))
        .and_then(|page| page.checked_mul(per_page))
        .ok_or(ReleaseError::Invalid("Release pagination is out of range"))?;
    let snapshot = db::releases::read_release_snapshot(
        pool,
        viewer,
        repo_id,
        db::releases::ReleaseSelection::Page {
            limit: per_page,
            offset,
        },
    )
    .await
    .map_err(write_error)?;
    let can_write = can_write(pool, viewer, repo_id).await?;
    Ok(ReleasesPage {
        repository: snapshot.repository,
        can_write,
        releases: snapshot.releases,
        assets: snapshot.assets,
        total: snapshot.total,
    })
}

/// One release by its tag. A draft is hidden from everyone but writers.
pub async fn detail_by_tag(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repo_id: i64,
    tag: &str,
) -> Result<ReleaseDetail, ReleaseError> {
    visible_detail(
        pool,
        viewer,
        repo_id,
        db::releases::ReleaseSelection::Tag(tag),
    )
    .await
}

/// One release by its number, for the JSON API. A draft is hidden from everyone but writers.
pub async fn detail_by_id(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repo_id: i64,
    release_id: i64,
) -> Result<ReleaseDetail, ReleaseError> {
    visible_detail(
        pool,
        viewer,
        repo_id,
        db::releases::ReleaseSelection::Id(release_id),
    )
    .await
}

async fn visible_detail(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repo_id: i64,
    selection: db::releases::ReleaseSelection<'_>,
) -> Result<ReleaseDetail, ReleaseError> {
    let mut snapshot = db::releases::read_release_snapshot(pool, viewer, repo_id, selection)
        .await
        .map_err(write_error)?;
    let release = snapshot.releases.pop().ok_or(ReleaseError::NotFound)?;
    let can_write = can_write(pool, viewer, repo_id).await?;
    Ok(ReleaseDetail {
        repository: snapshot.repository,
        can_write,
        release,
        assets: snapshot.assets,
    })
}

/// One release for a writer's edit page.
pub async fn edit_detail(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
) -> Result<ReleaseDetail, ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    let release = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    let assets = db::releases::list_release_assets(pool, release.release.id)
        .await
        .map_err(unavailable)?;
    Ok(ReleaseDetail {
        repository,
        can_write: true,
        release,
        assets,
    })
}

/// The tags and branches a writer can pick from on the new-release form.
pub async fn form_choices(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    owner: &str,
    name: &str,
) -> Result<FormChoices, ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    let store = repo_store(&repository, cluster);
    let info = store.info().await.map_err(ReleaseError::Git)?;
    // Only the names are shown, so the ref listing is enough; no tag or commit is read.
    let refs = store.refs().await.map_err(ReleaseError::Git)?;
    let mut tags: Vec<String> = refs
        .keys()
        .filter_map(|reference| reference.strip_prefix("refs/tags/"))
        .map(str::to_owned)
        .collect();
    tags.sort();
    Ok(FormChoices {
        repository,
        tags,
        branches: info.branches,
        default_branch: info.default_branch,
    })
}

/// The release count and the latest release the viewer may see, for the repository home.
pub async fn summary(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<i64>,
    repository: &Repository,
) -> Result<ReleaseSummary, ReleaseError> {
    let writer = can_write(pool, viewer, repository.id).await?;
    let count = db::releases::count_releases(pool, repository.id, writer)
        .await
        .map_err(unavailable)?;
    let latest = db::releases::latest_release(pool, repository.id)
        .await
        .map_err(unavailable)?;
    Ok(ReleaseSummary { count, latest })
}

/// Publishes a release. A new tag is created at the target first, then the release is recorded.
/// The tag exists in Git even if recording the release fails afterwards; the caller reports that.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn create(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    owner: &str,
    name: &str,
    input: &ReleaseInput,
) -> Result<Release, ReleaseError> {
    let (repository, user) = access::check_write(pool, input.repository_id, actor).await?;
    validate_tag(&input.tag)?;
    validate_fields(&input.fields)?;
    if db::releases::get_release_by_tag(pool, repository.id, &input.tag)
        .await
        .map_err(unavailable)?
        .is_some()
    {
        return Err(ReleaseError::TagTaken);
    }
    let store = repo_store(&repository, cluster);
    let mut refs = store.refs_from_primary().await.map_err(ReleaseError::Git)?;
    let existing = refs.remove(&format!("refs/tags/{}", input.tag));
    let mut draft_target = None;
    if existing.is_none() {
        if input.target.trim().is_empty() {
            return Err(ReleaseError::TargetRequired);
        }
        if input.target.trim().len() > 255 {
            return Err(ReleaseError::Invalid(
                "Release targets are limited to 255 bytes",
            ));
        }
        let commit = store
            .commit_from_primary(&target_revision(&refs, input.target.trim()))
            .await
            .map_err(ReleaseError::Git)?;
        // A draft gets its tag when it is published. Creating the tag now would make it
        // fetchable and start its tag workflows before the release is ready.
        if !input.fields.is_draft {
            if input.new_tag_denied {
                return Err(ReleaseError::Ref(RefError::Access(
                    AccessError::WriteDenied,
                )));
            }
            refs::create_tag(
                pool,
                cluster,
                ci,
                actor,
                owner,
                repository.id,
                &input.tag,
                &commit.sha,
                None,
            )
            .await
            .map_err(ReleaseError::from)?;
        } else {
            draft_target = Some(commit.sha);
        }
    }
    let attestation = immutable::candidate(
        pool,
        cluster,
        &repository,
        &input.tag,
        None,
        !input.fields.is_draft,
    )
    .await?;
    let (release, _) = store
        .mutate_release_as(
            pool,
            repository.id,
            actor,
            immutable::Mutation::Create {
                tag: input.tag.clone(),
                fields: input.fields.clone(),
                draft_target,
            },
            attestation,
        )
        .await
        .map_err(mutation_error)?;
    if !release.is_draft {
        notify_published(pool, broadcaster, &user, &repository, owner, name, &release).await;
    }
    let action = if release.is_draft {
        "created"
    } else {
        "published"
    };
    emit_release(pool, &repository, &release, actor, action, None, None).await;

    Ok(release)
}

/// A target that names an existing branch resolves to that branch, even when the branch name
/// starts with `refs/`, unless the target is itself an existing full ref. Any other target
/// resolves as an ordinary revision.
fn target_revision(refs: &HashMap<String, String>, target: &str) -> String {
    let branch = format!("refs/heads/{target}");
    if !refs.contains_key(target) && refs.contains_key(&branch) {
        branch
    } else {
        target.to_owned()
    }
}

/// Tells the repository's release webhooks what `actor` did to `release`. `changes` holds the
/// previous values of an edit.
async fn emit_release(
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
    release: &Release,
    actor: i64,
    action: &'static str,
    assets: Option<Vec<serde_json::Value>>,
    changes: Option<serde_json::Value>,
) {
    let (release, repository_for_hook) = (release.clone(), repository.clone());
    let author_id = release.author_id;
    webhooks::emit(
        pool,
        repository,
        events::RELEASE,
        vec![author_id, actor],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(actor));
            let mut payload = payloads::release(
                action,
                &release,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            );
            if let Some(assets) = assets {
                payload["release"]["assets"] = assets.into();
            }
            if let Some(changes) = changes {
                payload["changes"] = changes;
            }
            payload
        },
    )
    .await;
}

/// Changes a release's notes, title, flags and latest mark. A draft that is published now
/// notifies the watchers, as a release published at once does.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn update(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
    fields: &ReleaseChanges,
) -> Result<Release, ReleaseError> {
    let (repository, user) = writable(pool, actor, owner, name).await?;
    validate_fields(&ReleaseFields {
        name: fields.name.clone().flatten(),
        body: fields.body.clone().flatten(),
        ..Default::default()
    })?;
    let current = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?
        .release;
    // A stale form must not create the tag; the write below checks the same revision again.
    if fields
        .expected_revision
        .as_ref()
        .is_some_and(|expected| *expected != db::releases::release_revision(&current))
    {
        return Err(ReleaseError::Conflict);
    }
    let mut fields = fields.clone();
    // Whether this edit publishes, and so which checks run, is decided from this state.
    fields.expected_draft = Some(current.is_draft);
    let tag = fields
        .tag
        .as_deref()
        .unwrap_or(&current.tag_name)
        .to_owned();
    validate_tag(&tag)?;
    if current.is_immutable && tag != current.tag_name {
        return Err(write_error(ReleaseRefusal::Immutable.into()));
    }
    let publishing = current.is_draft && fields.is_draft == Some(false);
    let store = repo_store(&repository, cluster);
    if publishing || fields.tag.is_some() || fields.target.is_some() {
        // Pin the row revision across Git work; a concurrent edit must not change which tag
        // and target this publication was prepared for.
        fields
            .expected_revision
            .get_or_insert_with(|| db::releases::release_revision(&current));
        let refs = store.refs_from_primary().await.map_err(ReleaseError::Git)?;
        let reference = format!("refs/tags/{tag}");
        if !refs.contains_key(&reference) {
            let target = fields
                .target
                .as_deref()
                .unwrap_or(&current.target_commitish);
            if target.trim().is_empty() || target.len() > 255 {
                return Err(ReleaseError::TargetRequired);
            }
            let commit = store
                .commit_from_primary(&target_revision(&refs, target))
                .await
                .map_err(ReleaseError::Git)?;
            fields.target = Some(commit.sha.clone());
            if !fields.is_draft.unwrap_or(current.is_draft) {
                if fields.new_tag_denied {
                    return Err(ReleaseError::Ref(RefError::Access(
                        AccessError::WriteDenied,
                    )));
                }
                refs::create_tag(
                    pool,
                    cluster,
                    ci,
                    actor,
                    owner,
                    repository.id,
                    &tag,
                    &commit.sha,
                    None,
                )
                .await
                .map_err(ReleaseError::from)?;
            }
        } else {
            // An existing tag, not target_commitish, determines the release revision.
            fields.target = None;
        }
    }
    let attestation = immutable::candidate(
        pool,
        cluster,
        &repository,
        &tag,
        Some(release_id),
        publishing,
    )
    .await?;
    let (release, published) = store
        .mutate_release_as(
            pool,
            repository.id,
            actor,
            immutable::Mutation::Update {
                id: release_id,
                changes: fields.clone(),
            },
            attestation,
        )
        .await
        .map_err(mutation_error)?;
    if published {
        notify_published(pool, broadcaster, &user, &repository, owner, name, &release).await;
    }
    let action = if published {
        "published"
    } else if !current.is_draft && release.is_draft {
        "unpublished"
    } else if current.is_prerelease && !release.is_prerelease && !release.is_draft {
        "released"
    } else {
        "edited"
    };
    // Only an edit carries the earlier values, as on GitHub.
    let changes = (action == "edited").then(|| payloads::release_changes(&current, &release));
    emit_release(pool, &repository, &release, actor, action, None, changes).await;

    Ok(release)
}

/// Deletes a release and its assets. The uploaded files go too, unless something else uses them.
pub async fn delete(
    pool: &codeyard_db_core::DbPool,
    _store: &ObjectStore,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
) -> Result<(), ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    let deleted = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    // The deletion event still lists the assets, which are gone once the release is.
    let assets = webhooks::documents::release_assets(pool, &repository, owner, &deleted.release)
        .await
        .inspect_err(|error| tracing::warn!(release_id, %error, "release assets could not be read"))
        .ok();
    db::releases::delete_release(pool, actor, repository.id, release_id)
        .await
        .map_err(write_error)?;
    // The DAL queues asset bytes for deletion after the backup restore window.
    emit_release(
        pool,
        &repository,
        &deleted.release,
        actor,
        "deleted",
        assets,
        None,
    )
    .await;
    Ok(())
}

/// Attaches an uploaded file to a release. The bytes are checked and stored first, then the
/// row links them; a failed link removes the file again.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn add_asset(
    pool: &codeyard_db_core::DbPool,
    store: &ObjectStore,
    actor: i64,
    credential: &db::users::accounts::AccountCredential<'_>,
    owner: &str,
    name: &str,
    release_id: i64,
    filename: &str,
    label: Option<&str>,
    bytes: Vec<u8>,
) -> Result<ReleaseAsset, ReleaseError> {
    upload_asset(
        pool,
        store,
        actor,
        credential,
        owner,
        name,
        release_id,
        AssetUpload {
            token: None,
            name: filename,
            label,
            content_type: "application/octet-stream",
            bytes,
        },
    )
    .await
}

pub struct AssetUpload<'a> {
    pub token: Option<i64>,
    pub name: &'a str,
    pub label: Option<&'a str>,
    pub content_type: &'a str,
    pub bytes: Vec<u8>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn upload_asset(
    pool: &codeyard_db_core::DbPool,
    store: &ObjectStore,
    actor: i64,
    credential: &db::users::accounts::AccountCredential<'_>,
    owner: &str,
    name: &str,
    release_id: i64,
    input: AssetUpload<'_>,
) -> Result<ReleaseAsset, ReleaseError> {
    let AssetUpload {
        token,
        name: filename,
        label,
        content_type,
        bytes,
    } = input;
    let (repository, _) = writable(pool, actor, owner, name).await?;
    validate_asset_fields(filename, label)?;
    if bytes.is_empty() || bytes.len() > MAX_ASSET_BYTES {
        return Err(ReleaseError::Invalid("Assets are 1 byte to 100 MiB"));
    }
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let content_type = media_type.as_str();
    if content_type.len() > 100
        || !content_type.split_once('/').is_some_and(|(kind, subtype)| {
            !kind.is_empty() && !subtype.is_empty() && !subtype.contains('/')
        })
        || !content_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-+.".contains(&byte))
    {
        return Err(ReleaseError::Invalid("Invalid asset content type"));
    }
    let release = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    if release.release.is_immutable {
        return Err(ReleaseError::Invalid("The release's assets are immutable"));
    }
    let size = bytes.len() as i64;
    let checksum = hex::encode(Sha256::digest(&bytes));
    let file_id = files::store_upload(
        pool,
        store,
        actor,
        UploadDeclaration {
            filename,
            content_type,
            file_size: size,
            reason: codeyard_db_core::models::FileReason::ReleaseAsset,
            visibility: codeyard_db_core::models::FileVisibility::Owner,
            sha256_hex: &checksum,
        },
        bytes,
    )
    .await
    .map_err(ReleaseError::Upload)?;
    let linked = db::releases::add_release_asset_with_token(
        pool,
        actor,
        credential,
        repository.id,
        release_id,
        &db::releases::NewReleaseAsset {
            file_id,
            name: filename,
            label,
            content_type,
            size,
        },
        token,
    )
    .await
    .map_err(write_error);
    match linked {
        Ok(asset) => Ok(asset),
        Err(error) => {
            discard_file(pool, store, file_id).await;
            Err(error)
        }
    }
}

fn validate_asset_fields(filename: &str, label: Option<&str>) -> Result<(), ReleaseError> {
    if !codeyard_support::file_names::valid_basename(filename, 255) {
        return Err(ReleaseError::Invalid("Asset file name is not valid"));
    }
    if label.is_some_and(|label| label.contains('\0')) {
        return Err(ReleaseError::Invalid(
            "Asset labels cannot contain NUL characters",
        ));
    }
    if label.is_some_and(|label| label.len() > 255) {
        return Err(ReleaseError::Invalid(
            "Asset labels are limited to 255 bytes",
        ));
    }
    Ok(())
}

pub async fn update_asset(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    owner: &str,
    name: &str,
    asset_id: i64,
    changes: &db::releases::AssetChanges,
) -> Result<ReleaseAsset, ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    validate_asset_fields(
        changes.name.as_deref().unwrap_or("asset"),
        changes.label.as_ref().and_then(|label| label.as_deref()),
    )?;
    db::releases::update_release_asset(pool, actor, repository.id, asset_id, changes)
        .await
        .map_err(write_error)
}

/// Detaches an asset from its release and removes its file.
pub async fn delete_asset(
    pool: &codeyard_db_core::DbPool,
    _store: &ObjectStore,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
    asset_id: i64,
) -> Result<(), ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    db::releases::delete_release_asset(pool, actor, repository.id, release_id, asset_id)
        .await
        .map_err(write_error)?;
    Ok(())
}

/// Opens an asset for a reader of the repository. Drafts stay hidden from everyone but writers.
/// Only requests that send the body (`count`) add to the download count.
pub async fn download(
    pool: &codeyard_db_core::DbPool,
    store: &ObjectStore,
    viewer: Option<i64>,
    repo_id: i64,
    tag: &str,
    asset_name: &str,
    count: bool,
) -> Result<files::AssetDownload, ReleaseError> {
    let asset =
        db::releases::download_asset(pool, viewer, repo_id, Some(tag), Some(asset_name), None)
            .await
            .map_err(unavailable)?
            .ok_or(ReleaseError::NotFound)?;
    open_asset(pool, store, asset, count).await
}

/// Opens the asset with this id, which a later rename cannot point at another file.
pub async fn download_by_id(
    pool: &codeyard_db_core::DbPool,
    store: &ObjectStore,
    viewer: Option<i64>,
    repo_id: i64,
    asset_id: i64,
    count: bool,
) -> Result<files::AssetDownload, ReleaseError> {
    let asset = db::releases::download_asset(pool, viewer, repo_id, None, None, Some(asset_id))
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    open_asset(pool, store, asset, count).await
}

async fn open_asset(
    pool: &codeyard_db_core::DbPool,
    store: &ObjectStore,
    asset: ReleaseAsset,
    count: bool,
) -> Result<files::AssetDownload, ReleaseError> {
    let mut file = db::files::get_file_by_id(pool, asset.file_id)
        .await
        .map_err(unavailable)?
        .filter(|file| file.is_active)
        .ok_or(ReleaseError::NotFound)?;
    file.original_name = asset.name;
    let opened = files::open_file(store, file)
        .await
        .map_err(ReleaseError::Download)?;
    if count {
        if let Err(error) = db::releases::count_asset_download(pool, asset.id).await {
            tracing::warn!(%error, asset = asset.id, "could not count a release download");
        }
    }
    Ok(opened)
}

/// Removes an uploaded file that no release uses any more, as its uploader.
async fn discard_file(pool: &codeyard_db_core::DbPool, store: &ObjectStore, file: Uuid) {
    let uploader = match db::files::get_file_by_id(pool, file).await {
        Ok(Some(found)) => found.user_id,
        _ => None,
    };
    let Some(uploader) = uploader else {
        tracing::warn!(%file, "could not find the uploader of a release file");
        return;
    };
    if files::delete_unreferenced_file(pool, store, uploader, file)
        .await
        .is_err()
    {
        tracing::warn!(%file, "release file cleanup deferred");
    }
}

async fn notify_published(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &codeyard_db_core::models::User,
    repository: &Repository,
    owner: &str,
    name: &str,
    release: &Release,
) {
    let link = format!(
        "/{}/{}/releases/tag/{}",
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(name),
        codeyard_webui::encode_component(&release.tag_name)
    );
    notify_watchers(
        pool,
        broadcaster,
        WatchedItem {
            event: WatchEvent::Release,
            actor,
            repository,
            owner,
            repo: name,
            number: None,
            title: &release.tag_name,
            link: &link,
            notified: &[],
        },
    )
    .await;
}
