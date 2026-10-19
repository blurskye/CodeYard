//! Releases: a named, optionally annotated tag with notes and uploaded assets. Readers see the
//! published releases of a repository they can read; writers also see drafts and make changes.
//! The tag is created with the release when it does not exist yet.
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use crate::backend::{
    ci::CiRuns,
    cluster::NodeContext,
    db::{self, models::Release, models::ReleaseAsset, models::ReleaseRow, models::Repository},
    files::{self, DownloadError, UploadDeclaration, UploadError},
    git,
    notifications::{
        watchers::{notify_watchers, WatchEvent, WatchedItem},
        NotificationBroadcaster,
    },
    repositories::{
        access::{self, AccessError},
        refs::{self, RefError},
        store::{repo_store, RepoError},
    },
    storage::ObjectStore,
};
use anyhow::Error;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub use crate::backend::db::releases::{ReleaseFields, ReleaseRefusal};

pub const MAX_ASSET_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug)]
pub enum ReleaseError {
    Access(AccessError),
    Invalid(&'static str),
    NotFound,
    /// The tag is already a release.
    TagTaken,
    /// The release already has an asset with this name.
    AssetNameTaken,
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

/// What a writer sends to publish a release. The tag and the target only matter when the tag
/// does not exist yet: then it is created at `target`, a branch or commit.
#[derive(Debug, Clone, Default)]
pub struct ReleaseInput {
    pub tag: String,
    pub target: String,
    pub fields: ReleaseFields,
}

pub struct ReleasesPage {
    pub repository: Repository,
    pub can_write: bool,
    pub releases: Vec<ReleaseRow>,
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

async fn readable(
    pool: &db::DbPool,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
) -> Result<Repository, ReleaseError> {
    let repository = db::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| ReleaseError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    Ok(access::check_read(pool, repository.id, viewer).await?)
}

async fn writable(
    pool: &db::DbPool,
    actor: i64,
    owner: &str,
    name: &str,
) -> Result<(Repository, db::models::User), ReleaseError> {
    let repository = db::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| ReleaseError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    Ok(access::check_write(pool, repository.id, actor).await?)
}

async fn can_write(pool: &db::DbPool, viewer: Option<i64>, repo_id: i64) -> bool {
    match viewer {
        Some(actor) => access::check_write(pool, repo_id, actor).await.is_ok(),
        None => false,
    }
}

fn unavailable<E>(_: E) -> ReleaseError {
    ReleaseError::Unavailable
}

/// Maps a refusal the database raised inside a write to the error the caller shows.
fn write_error(error: Error) -> ReleaseError {
    match error.downcast_ref::<ReleaseRefusal>() {
        Some(ReleaseRefusal::Denied) => ReleaseError::Access(AccessError::WriteDenied),
        Some(ReleaseRefusal::NotFound) => ReleaseError::NotFound,
        Some(ReleaseRefusal::TagTaken) => ReleaseError::TagTaken,
        Some(ReleaseRefusal::AssetNameTaken) => ReleaseError::AssetNameTaken,
        None => ReleaseError::Unavailable,
    }
}

fn validate_fields(fields: &ReleaseFields) -> Result<(), ReleaseError> {
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
    pool: &db::DbPool,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
    page: i64,
    per_page: i64,
) -> Result<ReleasesPage, ReleaseError> {
    let repository = readable(pool, viewer, owner, name).await?;
    let can_write = can_write(pool, viewer, repository.id).await;
    let releases = db::releases::list_releases(
        pool,
        repository.id,
        can_write,
        per_page,
        (page - 1).max(0) * per_page,
    )
    .await
    .map_err(unavailable)?;
    let total = db::releases::count_releases(pool, repository.id, can_write)
        .await
        .map_err(unavailable)?;
    Ok(ReleasesPage {
        repository,
        can_write,
        releases,
        total,
    })
}

/// One release by its tag. A draft is hidden from everyone but writers.
pub async fn detail_by_tag(
    pool: &db::DbPool,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
    tag: &str,
) -> Result<ReleaseDetail, ReleaseError> {
    let repository = readable(pool, viewer, owner, name).await?;
    let can_write = can_write(pool, viewer, repository.id).await;
    let release = db::releases::get_release_by_tag(pool, repository.id, tag)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    if release.release.is_draft && !can_write {
        return Err(ReleaseError::NotFound);
    }
    let assets = db::releases::list_release_assets(pool, release.release.id)
        .await
        .map_err(unavailable)?;
    Ok(ReleaseDetail {
        repository,
        can_write,
        release,
        assets,
    })
}

/// One release by its number, for the JSON API. A draft is hidden from everyone but writers.
pub async fn detail_by_id(
    pool: &db::DbPool,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
    release_id: i64,
) -> Result<ReleaseDetail, ReleaseError> {
    let repository = readable(pool, viewer, owner, name).await?;
    let can_write = can_write(pool, viewer, repository.id).await;
    let release = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    if release.release.is_draft && !can_write {
        return Err(ReleaseError::NotFound);
    }
    let assets = db::releases::list_release_assets(pool, release.release.id)
        .await
        .map_err(unavailable)?;
    Ok(ReleaseDetail {
        repository,
        can_write,
        release,
        assets,
    })
}

/// One release for a writer's edit page.
pub async fn edit_detail(
    pool: &db::DbPool,
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
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    owner: &str,
    name: &str,
) -> Result<FormChoices, ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    let store = repo_store(&repository, cluster);
    let info = store.info().await.map_err(ReleaseError::Git)?;
    let mut tags: Vec<String> = store
        .tags()
        .await
        .map_err(ReleaseError::Git)?
        .into_iter()
        .map(|tag| tag.name)
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
    pool: &db::DbPool,
    viewer: Option<i64>,
    repository: &Repository,
) -> Result<ReleaseSummary, ReleaseError> {
    let writer = can_write(pool, viewer, repository.id).await;
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
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    owner: &str,
    name: &str,
    input: &ReleaseInput,
) -> Result<Release, ReleaseError> {
    let (repository, user) = writable(pool, actor, owner, name).await?;
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
    let existing = store
        .tags()
        .await
        .map_err(ReleaseError::Git)?
        .into_iter()
        .find(|tag| tag.name == input.tag);
    let target = match existing {
        Some(tag) => tag.sha,
        None => {
            if input.target.trim().is_empty() {
                return Err(ReleaseError::TargetRequired);
            }
            let commit = store
                .commit(input.target.trim())
                .await
                .map_err(ReleaseError::Git)?;
            refs::create_tag(
                pool,
                cluster,
                ci,
                actor,
                owner,
                name,
                &input.tag,
                &commit.sha,
                None,
            )
            .await
            .map_err(ReleaseError::Ref)?;
            input.target.trim().to_owned()
        }
    };
    let release = db::releases::create_release(
        pool,
        actor,
        repository.id,
        &input.tag,
        &target,
        &input.fields,
    )
    .await
    .map_err(write_error)?;
    if !release.is_draft {
        notify_published(pool, broadcaster, &user, &repository, owner, name, &release).await;
    }
    let action = if release.is_draft {
        "created"
    } else {
        "published"
    };
    let (release_for_hook, repository_for_hook) = (release.clone(), repository.clone());
    let (author_id, sender_id) = (actor, actor);
    webhooks::emit(
        pool,
        &repository,
        events::RELEASE,
        vec![author_id, sender_id],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(sender_id));
            payloads::release(
                action,
                &release_for_hook,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );

    Ok(release)
}

/// Changes a release's notes, title, flags and latest mark. A draft that is published now
/// notifies the watchers, as a release published at once does.
pub async fn update(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
    fields: &ReleaseFields,
) -> Result<Release, ReleaseError> {
    let (repository, user) = writable(pool, actor, owner, name).await?;
    validate_fields(fields)?;
    let before = db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    let release = db::releases::update_release(pool, actor, repository.id, release_id, fields)
        .await
        .map_err(write_error)?;
    if before.release.is_draft && !release.is_draft {
        notify_published(pool, broadcaster, &user, &repository, owner, name, &release).await;
    }
    let action = if before.release.is_draft && !release.is_draft {
        "published"
    } else {
        "edited"
    };
    let (release_for_hook, repository_for_hook) = (release.clone(), repository.clone());
    let (author_id, sender_id) = (actor, actor);
    webhooks::emit(
        pool,
        &repository,
        events::RELEASE,
        vec![author_id, sender_id],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(sender_id));
            payloads::release(
                action,
                &release_for_hook,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );

    Ok(release)
}

/// Deletes a release and its assets. The uploaded files go too, unless something else uses them.
pub async fn delete(
    pool: &db::DbPool,
    store: &ObjectStore,
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
    let files = db::releases::delete_release(pool, actor, repository.id, release_id)
        .await
        .map_err(write_error)?;
    for file in files {
        discard_file(pool, store, file).await;
    }
    let deleted = deleted.release;
    let (release_for_hook, repository_for_hook) = (deleted.clone(), repository.clone());
    let (author_id, sender_id) = (actor, actor);
    webhooks::emit(
        pool,
        &repository,
        events::RELEASE,
        vec![author_id, sender_id],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(sender_id));
            payloads::release(
                "deleted",
                &release_for_hook,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );
    Ok(())
}

/// Attaches an uploaded file to a release. The bytes are checked and stored first, then the
/// row links them; a failed link removes the file again.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn add_asset(
    pool: &db::DbPool,
    store: &ObjectStore,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
    filename: &str,
    label: Option<&str>,
    bytes: Vec<u8>,
) -> Result<ReleaseAsset, ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    if filename.is_empty()
        || filename.len() > 255
        || filename.contains(['/', '\\'])
        || filename.chars().any(char::is_control)
    {
        return Err(ReleaseError::Invalid("Asset file name is not valid"));
    }
    if label.is_some_and(|label| label.len() > 255) {
        return Err(ReleaseError::Invalid(
            "Asset labels are limited to 255 bytes",
        ));
    }
    if bytes.is_empty() || bytes.len() > MAX_ASSET_BYTES {
        return Err(ReleaseError::Invalid("Assets are 1 byte to 100 MiB"));
    }
    db::releases::get_release(pool, repository.id, release_id)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    let size = bytes.len() as i64;
    let checksum = hex::encode(Sha256::digest(&bytes));
    let content_type = "application/octet-stream";
    let file_id = files::store_upload(
        pool,
        store,
        actor,
        UploadDeclaration {
            filename,
            content_type,
            file_size: size,
            reason: db::models::FileReason::ReleaseAsset,
            visibility: db::models::FileVisibility::Owner,
            sha256_hex: &checksum,
        },
        bytes,
    )
    .await
    .map_err(ReleaseError::Upload)?;
    let linked = async {
        if !db::files::activate_upload(pool, file_id, actor)
            .await
            .map_err(unavailable)?
        {
            return Err(ReleaseError::Unavailable);
        }
        db::releases::add_release_asset(
            pool,
            actor,
            repository.id,
            release_id,
            &db::releases::NewReleaseAsset {
                file_id,
                name: filename,
                label,
                content_type,
                size,
            },
        )
        .await
        .map_err(write_error)
    }
    .await;
    match linked {
        Ok(asset) => Ok(asset),
        Err(error) => {
            discard_file(pool, store, file_id).await;
            Err(error)
        }
    }
}

/// Detaches an asset from its release and removes its file.
pub async fn delete_asset(
    pool: &db::DbPool,
    store: &ObjectStore,
    actor: i64,
    owner: &str,
    name: &str,
    release_id: i64,
    asset_id: i64,
) -> Result<(), ReleaseError> {
    let (repository, _) = writable(pool, actor, owner, name).await?;
    let file = db::releases::delete_release_asset(pool, actor, repository.id, release_id, asset_id)
        .await
        .map_err(write_error)?;
    discard_file(pool, store, file).await;
    Ok(())
}

/// Opens an asset for a reader of the repository. Drafts stay hidden from everyone but writers.
pub async fn download(
    pool: &db::DbPool,
    store: &ObjectStore,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
    tag: &str,
    asset_name: &str,
) -> Result<files::AssetDownload, ReleaseError> {
    let repository = readable(pool, viewer, owner, name).await?;
    let release = db::releases::get_release_by_tag(pool, repository.id, tag)
        .await
        .map_err(unavailable)?
        .ok_or(ReleaseError::NotFound)?;
    if release.release.is_draft && !can_write(pool, viewer, repository.id).await {
        return Err(ReleaseError::NotFound);
    }
    let assets = db::releases::list_release_assets(pool, release.release.id)
        .await
        .map_err(unavailable)?;
    let asset = assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .ok_or(ReleaseError::NotFound)?;
    let file = db::files::get_file_by_id(pool, asset.file_id)
        .await
        .map_err(unavailable)?
        .filter(|file| file.is_active)
        .ok_or(ReleaseError::NotFound)?;
    let opened = files::open_file(store, file)
        .await
        .map_err(ReleaseError::Download)?;
    if let Err(error) = db::releases::count_asset_download(pool, asset.id).await {
        tracing::warn!(%error, asset = asset.id, "could not count a release download");
    }
    Ok(opened)
}

/// Removes an uploaded file that no release uses any more, as its uploader.
async fn discard_file(pool: &db::DbPool, store: &ObjectStore, file: Uuid) {
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
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &db::models::User,
    repository: &Repository,
    owner: &str,
    name: &str,
    release: &Release,
) {
    let link = format!(
        "/{}/{}/releases/tag/{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name),
        crate::webui::encode_component(&release.tag_name)
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
