//! Release pages and their native forms. Each form posts back to its page; the domain decides
//! who may write, and a refused form is shown as it was submitted, with the reason.
use axum::extract::multipart::MultipartError;
use axum::extract::Form;
use axum::extract::Multipart;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::files::download::asset_response;
use codeyard_api_http::shared::repos::code::upload::READ_TIMEOUT as UPLOAD_READ_TIMEOUT;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_db_core::models::ReleaseAsset;
use codeyard_db_core::models::ReleaseRow;
use codeyard_db_repos::releases::ReleaseFields;
use codeyard_git::error::ErrorKind;
use codeyard_identity::repository_access::AccessError;
use codeyard_repos_core::ref_errors::RefError;
use codeyard_repos_core::repositories::releases::ReleaseError;
use codeyard_repos_pulls::repositories::releases::ReleaseInput;
use codeyard_storage::files::downloads::DownloadError;
use codeyard_storage::files::uploads::UploadError;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::chrome::RepoChrome;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use codeyard_webui::pages::repos::releases::AssetItem;
use codeyard_webui::pages::repos::releases::Page as ReleasePage;
use codeyard_webui::pages::repos::releases::ReleaseDraft;
use codeyard_webui::pages::repos::releases::ReleaseItem;
use std::collections::HashMap;
use std::sync::Arc;

const PER_PAGE: i64 = 30;

/// An asset upload carries the file, the CSRF token and the label, so a form with more parts or
/// a longer text field is not one of ours.
const MAX_UPLOAD_PARTS: usize = 8;
const MAX_UPLOAD_FIELD_BYTES: usize = 4096;

/// The list shows no notes, so it asks for the item without them.
fn release_item(row: &ReleaseRow, with_body: bool) -> ReleaseItem {
    let release = &row.release;
    ReleaseItem {
        id: release.id,
        tag: release.tag_name.clone(),
        title: release
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        body: if with_body {
            release.body.clone().unwrap_or_default()
        } else {
            String::new()
        },
        author: row.author.clone(),
        target: release.target_commitish.clone(),
        archive_revision: crate::releases::json::archive_revision(release),
        created_at: release.created_at.to_rfc3339(),
        published_at: release.published_at.map(|date| date.to_rfc3339()),
        draft: release.is_draft,
        immutable: release.is_immutable,
        prerelease: release.is_prerelease,
        latest: release.is_latest,
    }
}

fn asset_item(repo_base: &str, tag: &str, asset: &ReleaseAsset) -> AssetItem {
    AssetItem {
        id: asset.id,
        name: asset.name.clone(),
        label: asset.label.clone().unwrap_or_default(),
        size: asset.size,
        downloads: asset.download_count,
        href: format!(
            "{repo_base}/releases/download/{}/{}",
            codeyard_types::paths::encode_component(tag),
            codeyard_types::paths::encode_component(&asset.name)
        ),
    }
}

/// The status and the text a refused release operation shows.
pub(super) fn refused(error: &ReleaseError) -> (StatusCode, String) {
    let (status, message) = match error {
        ReleaseError::Access(AccessError::NotFound) | ReleaseError::NotFound => {
            (StatusCode::NOT_FOUND, "Release not found.")
        }
        ReleaseError::Access(AccessError::WriteDenied | AccessError::PasswordChangeRequired) => (
            StatusCode::FORBIDDEN,
            "You do not have permission to manage releases here.",
        ),
        ReleaseError::Access(_) | ReleaseError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Release storage is unavailable. Try again.",
        ),
        ReleaseError::Invalid(message) => (StatusCode::UNPROCESSABLE_ENTITY, *message),
        ReleaseError::Conflict => (StatusCode::CONFLICT, "The release changed. Your draft is preserved; review the latest release before retrying."),
        ReleaseError::TagTaken => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "A release already uses this tag.",
        ),
        ReleaseError::AssetNameTaken => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "This release already has an asset with this name.",
        ),
        ReleaseError::TargetRequired => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a branch or commit for the new tag.",
        ),
        ReleaseError::Ref(RefError::Invalid(message) | RefError::Protected(message)) => {
            (StatusCode::UNPROCESSABLE_ENTITY, *message)
        }
        ReleaseError::Ref(RefError::Access(_)) => (
            StatusCode::FORBIDDEN,
            "You do not have permission to manage releases here.",
        ),
        ReleaseError::Git(error)|ReleaseError::Ref(RefError::Git(error)) if matches!(error.kind,ErrorKind::Invalid|ErrorKind::Conflict) && !error.is_response_too_large() => (if error.kind==ErrorKind::Invalid {StatusCode::UNPROCESSABLE_ENTITY}else{StatusCode::CONFLICT},error.message.as_str()),
        ReleaseError::Ref(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The tag could not be created. Try again.",
        ),
        ReleaseError::Git(error)
            if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) =>
        {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "The branch or commit was not found.",
            )
        }
        ReleaseError::Git(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository storage is unavailable. Try again.",
        ),
        ReleaseError::Upload(UploadError::Invalid) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "The file could not be accepted. Check its size and name.",
        ),
        ReleaseError::Upload(UploadError::Quota) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Your upload quota is used up.",
        ),
        ReleaseError::Upload(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The file could not be stored. Try again.",
        ),
        ReleaseError::Download(DownloadError::NotFound) => {
            (StatusCode::NOT_FOUND, "Asset not found.")
        }
        ReleaseError::Download(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The file is unavailable right now.",
        ),
    };
    (status, message.to_owned())
}

/// The error page of a refused operation, in the repository's chrome.
fn refusal(ctx: &PageContext, repo: &RepoChrome, error: ReleaseError) -> Response {
    let (status, message) = refused(&error);
    ctx.repo_error(repo, status, &message)
}

fn repo_page(repo: RepoChrome, page: ReleasePage) -> Page {
    Page::Repositories(RepoPage::Releases { repo, page })
}

/// The release form as submitted. A checkbox is in the form only when it is ticked.
fn submitted_draft(form: &HashMap<String, String>) -> ReleaseDraft {
    ReleaseDraft {
        revision: field(form, "revision"),
        tag: field(form, "tag"),
        target: field(form, "target"),
        title: field(form, "title"),
        body: field(form, "body"),
        prerelease: form.contains_key("prerelease"),
        draft: form.contains_key("draft"),
        latest: form.contains_key("latest"),
        preview: field(form, "intent") == "preview",
        previous_tag: field(form, "previous_tag"),
        configuration_path: field(form, "configuration_path"),
    }
}

fn fields_of(draft: &ReleaseDraft) -> ReleaseFields {
    let title = draft.title.trim();
    ReleaseFields {
        name: (!title.is_empty()).then(|| title.to_owned()),
        body: (!draft.body.trim().is_empty()).then(|| draft.body.clone()),
        is_draft: draft.draft,
        is_prerelease: draft.prerelease,
        make_latest: draft.latest.into(),
    }
}

/// The new-release form, with what was typed and the reason it was refused.
async fn new_form(
    ctx: &PageContext,
    repo: &RepoChrome,
    draft: ReleaseDraft,
    error: Option<String>,
) -> Response {
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to publish a release.");
    };
    let (tags, mut branches, default_branch) =
        match codeyard_repos_pulls::repositories::releases::form_choices(
            &ctx.state.pool,
            ctx.state.cluster.as_ref(),
            actor,
            &repo.owner,
            &repo.name,
        )
        .await
        {
            Ok(choices) => (choices.tags, choices.branches, choices.default_branch),
            Err(ReleaseError::Git(_) | ReleaseError::Unavailable) if error.is_some() => {
                (Vec::new(), Vec::new(), draft.target.clone())
            }
            Err(error) => return refusal(ctx, repo, error),
        };
    // A target that vanished stays the selected option, so a retry fails again instead of
    // quietly tagging another branch.
    if !draft.target.is_empty() && !branches.is_empty() && !branches.contains(&draft.target) {
        branches.push(draft.target.clone());
    }
    ctx.page_model(
        "New release",
        repo_page(
            repo.clone(),
            ReleasePage::Form {
                editing: None,
                immutable: false,
                draft,
                tags,
                branches,
                default_branch,
                assets: Vec::new(),
                error,
            },
        ),
    )
}

/// The edit form of a release, with the assets as they are stored. A refused edit passes the
/// draft the writer typed, so nothing they entered is lost. Only a conflict renews the draft's
/// revision: the writer has been told the release changed, so submitting again overwrites it.
async fn edit_form(
    ctx: &PageContext,
    repo: &RepoChrome,
    owner: &str,
    name: &str,
    id: i64,
    draft: Option<ReleaseDraft>,
    failure: Option<&ReleaseError>,
) -> Response {
    let conflict = matches!(failure, Some(ReleaseError::Conflict));
    let error = failure.map(|failure| refused(failure).1);
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to edit a release.");
    };
    let found = match codeyard_repos_pulls::repositories::releases::edit_detail(
        &ctx.state.pool,
        actor,
        owner,
        name,
        id,
    )
    .await
    {
        Ok(found) => found,
        Err(error) => return refusal(ctx, repo, error),
    };
    let base = repo.base();
    let tag = found.release.release.tag_name.clone();
    let mut draft = draft.unwrap_or_else(|| ReleaseDraft {
        revision: codeyard_db_repos::releases::release_revision(&found.release.release),
        tag: tag.clone(),
        target: found.release.release.target_commitish.clone(),
        title: found.release.release.name.clone().unwrap_or_default(),
        body: found.release.release.body.clone().unwrap_or_default(),
        prerelease: found.release.release.is_prerelease,
        draft: found.release.release.is_draft,
        latest: found.release.release.is_latest,
        preview: false,
        ..Default::default()
    });
    // The form shows the tag as fixed text, so a submitted draft carries none.
    draft.tag = tag.clone();
    draft.target = found.release.release.target_commitish.clone();
    if conflict {
        draft.revision = codeyard_db_repos::releases::release_revision(&found.release.release);
    }
    let assets = found
        .assets
        .iter()
        .map(|asset| asset_item(&base, &tag, asset))
        .collect();
    ctx.page_model(
        "Edit release",
        repo_page(
            repo.clone(),
            ReleasePage::Form {
                editing: Some(id),
                immutable: found.release.release.is_immutable,
                draft,
                tags: Vec::new(),
                branches: Vec::new(),
                default_branch: String::new(),
                assets,
                error,
            },
        ),
    )
}

pub(crate) async fn list(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let page = ctx
        .query
        .get("page")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match codeyard_repos_pulls::repositories::releases::list(
        &ctx.state.pool,
        viewer,
        repo.id,
        page,
        PER_PAGE,
    )
    .await
    {
        Ok(listing) => {
            let items = listing
                .releases
                .iter()
                .map(|row| release_item(row, false))
                .collect();
            ctx.page_model(
                "Releases",
                repo_page(
                    repo,
                    ReleasePage::List {
                        items,
                        total: listing.total,
                        page,
                        per_page: PER_PAGE,
                        can_write: listing.can_write,
                    },
                ),
            )
        }
        Err(error) => refusal(&ctx, &repo, error),
    }
}

pub(crate) async fn detail(
    State(state): State<Arc<AppState>>,
    Path((owner, name, tag)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match codeyard_repos_pulls::repositories::releases::detail_by_tag(
        &ctx.state.pool,
        viewer,
        repo.id,
        &tag,
    )
    .await
    {
        Ok(found) => {
            let base = repo.base();
            let release = release_item(&found.release, true);
            let assets = found
                .assets
                .iter()
                .map(|asset| asset_item(&base, &release.tag, asset))
                .collect();
            let counts = match codeyard_db_repos::reactions::reaction_counts(
                &ctx.state.pool,
                "release",
                &[release.id],
                viewer.unwrap_or(0),
            )
            .await
            {
                Ok(counts) => counts,
                Err(_) => {
                    return ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "The reactions could not be loaded.",
                    )
                }
            };
            let reactions = counts
                .into_iter()
                .map(|count| codeyard_webui::pages::repos::issues::Reaction {
                    content: count.content,
                    count: count.count,
                    mine: count.mine,
                })
                .collect();
            let title = release.title.clone();
            ctx.page_model(
                &title,
                repo_page(
                    repo,
                    ReleasePage::Detail {
                        release,
                        assets,
                        reactions,
                        can_write: found.can_write,
                    },
                ),
            )
        }
        Err(error) => refusal(&ctx, &repo, error),
    }
}

pub(crate) async fn new_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let draft = ReleaseDraft {
        latest: true,
        ..ReleaseDraft::default()
    };
    new_form(&ctx, &repo, draft, None).await
}

pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let draft = submitted_draft(&form);
    if form
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(repo.id)
    {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload the page and try again.",
        );
    }
    if draft.preview {
        return new_form(&ctx, &repo, draft, None).await;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to publish a release.");
    };
    if field(&form, "intent") == "generate-notes" {
        let generated = codeyard_repos_pulls::repositories::releases::notes::generate_notes(
            &ctx.state.pool,
            ctx.state.cluster.as_ref(),
            actor,
            repo.id,
            &codeyard_repos_pulls::repositories::releases::notes::NotesInput {
                tag: draft.tag.trim(),
                target: (!draft.target.trim().is_empty()).then_some(draft.target.trim()),
                previous_tag: (!draft.previous_tag.trim().is_empty())
                    .then_some(draft.previous_tag.trim()),
                configuration_path: (!draft.configuration_path.trim().is_empty())
                    .then_some(draft.configuration_path.trim()),
            },
        )
        .await;
        let notes = match generated {
            Ok(notes) => notes,
            Err(error) => {
                let (status, message) = refused(&error);
                return with_status(new_form(&ctx, &repo, draft, Some(message)).await, status);
            }
        };
        let body = match notes.with_preface(Some(&draft.body)) {
            Ok(body) => body,
            Err(error) => {
                let (status, message) = refused(&error);
                return with_status(new_form(&ctx, &repo, draft, Some(message)).await, status);
            }
        };
        let mut draft = draft;
        draft.body = body;
        if draft.title.trim().is_empty() {
            draft.title = notes.name;
        }
        return new_form(&ctx, &repo, draft, None).await;
    }
    let input = ReleaseInput {
        repository_id: repo.id,
        tag: draft.tag.trim().to_owned(),
        target: draft.target.trim().to_owned(),
        fields: fields_of(&draft),
        new_tag_denied: false,
    };
    match codeyard_repos_pulls::repositories::releases::create(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.ci_runs,
        &ctx.state.notification_broadcaster,
        actor,
        &owner,
        &name,
        &input,
    )
    .await
    {
        Ok(release) => ctx.redirect(&format!(
            "{}/releases/tag/{}",
            repo.base(),
            codeyard_types::paths::encode_component(&release.tag_name)
        )),
        Err(error) => {
            let (status, message) = refused(&error);
            with_status(new_form(&ctx, &repo, draft, Some(message)).await, status)
        }
    }
}

pub(crate) async fn edit_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    edit_form(&ctx, &repo, &owner, &name, id, None, None).await
}

pub(crate) async fn update(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let draft = submitted_draft(&form);
    if draft.preview {
        return edit_form(&ctx, &repo, &owner, &name, id, Some(draft), None).await;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to edit a release.");
    };
    let mut changes: codeyard_db_repos::releases::ReleaseChanges = (&fields_of(&draft)).into();
    changes.expected_revision = Some(draft.revision.clone());
    match codeyard_repos_pulls::repositories::releases::update(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.ci_runs,
        &ctx.state.notification_broadcaster,
        actor,
        &owner,
        &name,
        id,
        &changes,
    )
    .await
    {
        Ok(release) => ctx.redirect(&format!(
            "{}/releases/tag/{}",
            repo.base(),
            codeyard_types::paths::encode_component(&release.tag_name)
        )),
        Err(error) => {
            let (status, _) = refused(&error);
            with_status(
                edit_form(&ctx, &repo, &owner, &name, id, Some(draft), Some(&error)).await,
                status,
            )
        }
    }
}

pub(crate) async fn delete(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to delete a release.");
    };
    match codeyard_repos_pulls::repositories::releases::delete(
        &ctx.state.pool,
        &ctx.state.storage,
        actor,
        &owner,
        &name,
        id,
    )
    .await
    {
        Ok(()) => ctx.redirect(&format!("{}/releases", repo.base())),
        Err(error) => refusal(&ctx, &repo, error),
    }
}

/// A body over the limit is a 413. Any other read failure is a bad request.
fn upload_read_error(ctx: &PageContext, error: &MultipartError) -> Response {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ctx.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Assets are limited to 100 MiB.",
        )
    } else {
        ctx.error(StatusCode::BAD_REQUEST, "The upload could not be read.")
    }
}

/// Attaches the uploaded file to a release. The other form fields carry the CSRF token and the
/// label.
pub(crate) async fn upload_asset(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    mut multipart: Multipart,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to upload an asset.");
    };
    if let Err(error) = codeyard_repos_pulls::repositories::releases::edit_detail(
        &ctx.state.pool,
        actor,
        &owner,
        &name,
        id,
    )
    .await
    {
        let (status, message) = refused(&error);
        return ctx.error(status, &message);
    }
    let Ok(_slot) = ctx.state.upload_reads.clone().try_acquire_owned() else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Uploads are busy. Try again in a moment.",
        );
    };
    let read = async {
        let mut fields = HashMap::new();
        let mut file = None;
        let mut parts = 0;
        loop {
            let mut part = match multipart.next_field().await {
                Ok(Some(part)) => part,
                Ok(None) => break,
                Err(error) => return Err(upload_read_error(&ctx, &error)),
            };
            parts += 1;
            if parts > MAX_UPLOAD_PARTS {
                return Err(ctx.error(StatusCode::BAD_REQUEST, "The upload could not be read."));
            }
            let part_name = part.name().unwrap_or_default().to_owned();
            if part_name == "file" {
                let filename = part.file_name().unwrap_or_default().to_owned();
                match part.bytes().await {
                    Ok(bytes) => file = Some((filename, bytes.to_vec())),
                    Err(error) => return Err(upload_read_error(&ctx, &error)),
                }
            } else {
                match codeyard_api_http::multipart::bounded_text(&mut part, MAX_UPLOAD_FIELD_BYTES)
                    .await
                {
                    Ok(Some(text)) => {
                        fields.insert(part_name, text);
                    }
                    Ok(None) => {
                        return Err(
                            ctx.error(StatusCode::BAD_REQUEST, "The upload could not be read.")
                        )
                    }
                    Err(error) => return Err(upload_read_error(&ctx, &error)),
                }
            }
        }
        Ok((fields, file))
    };
    let (fields, file) = match tokio::time::timeout(UPLOAD_READ_TIMEOUT, read).await {
        Ok(Ok(parts)) => parts,
        Ok(Err(response)) => return response,
        Err(_) => {
            return ctx.error(
                StatusCode::REQUEST_TIMEOUT,
                "The upload timed out. Choose the file again.",
            )
        }
    };
    if let Err(response) = authorize_post(&ctx, &fields) {
        return response;
    }
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to upload an asset.");
    };
    let Some((filename, bytes)) = file else {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Choose a file to upload.");
    };
    let label = field(&fields, "label");
    let label = label.trim();
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match codeyard_repos_pulls::repositories::releases::add_asset(
        &ctx.state.pool,
        &ctx.state.storage,
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session),
        &owner,
        &name,
        id,
        &filename,
        (!label.is_empty()).then_some(label),
        bytes,
    )
    .await
    {
        Ok(_) => ctx.redirect(&format!("{}/releases/edit/{id}", repo.base())),
        Err(error) => refusal(&ctx, &repo, error),
    }
}

pub(crate) async fn update_asset(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id, asset)): Path<(String, String, i64, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to edit an asset.");
    };
    match codeyard_db_repos::releases::get_repository_release_asset(
        &ctx.state.pool,
        loaded.repository.id,
        asset,
    )
    .await
    {
        Ok(Some(asset)) if asset.release_id == id => {}
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "Asset not found."),
        Err(_) => return refusal(&ctx, &loaded.chrome, ReleaseError::Unavailable),
    }
    let label = field(&form, "label");
    let label = label.trim();
    let changes = codeyard_db_repos::releases::AssetChanges {
        name: Some(field(&form, "name").to_owned()),
        label: Some((!label.is_empty()).then(|| label.to_owned())),
    };
    match codeyard_repos_pulls::repositories::releases::update_asset(
        &ctx.state.pool,
        actor,
        &owner,
        &name,
        asset,
        &changes,
    )
    .await
    {
        Ok(_) => ctx.redirect(&format!("{}/releases/edit/{id}", loaded.chrome.base())),
        Err(error) => refusal(&ctx, &loaded.chrome, error),
    }
}

pub(crate) async fn delete_asset(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id, asset)): Path<(String, String, i64, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let repo =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded.chrome,
            Err(response) => return response,
        };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to remove an asset.");
    };
    match codeyard_repos_pulls::repositories::releases::delete_asset(
        &ctx.state.pool,
        &ctx.state.storage,
        actor,
        &owner,
        &name,
        id,
        asset,
    )
    .await
    {
        Ok(()) => ctx.redirect(&format!("{}/releases/edit/{id}", repo.base())),
        Err(error) => refusal(&ctx, &repo, error),
    }
}

/// `/releases/download/{tag}/{asset}`. A tag may contain slashes, so the asset is the last part.
pub(crate) async fn download(
    State(state): State<Arc<AppState>>,
    method: axum::http::Method,
    Path((owner, name, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let Some((tag, asset)) = rest.rsplit_once('/') else {
        return ctx.error(StatusCode::NOT_FOUND, "Asset not found.");
    };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match codeyard_repos_pulls::repositories::releases::download(
        &ctx.state.pool,
        &ctx.state.storage,
        viewer,
        loaded.repository.id,
        tag,
        asset,
        method != axum::http::Method::HEAD,
    )
    .await
    {
        Ok(opened) => match asset_response(opened, method) {
            Ok(response) => ctx.with_cookie(response),
            Err(failure) => failure.into_response(),
        },
        Err(error) => {
            match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await
            {
                Ok(loaded) => refusal(&ctx, &loaded.chrome, error),
                Err(response) => response,
            }
        }
    }
}
