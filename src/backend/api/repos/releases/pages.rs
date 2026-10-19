//! Release pages and their native forms. Each form posts back to its page; the domain decides
//! who may write, and a refused form is shown as it was submitted, with the reason.
use crate::backend::api::files::download::asset_response;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome;
use crate::backend::api::repos::pulls::pull::{authorize_post, with_status};
use crate::backend::app::AppState;
use crate::backend::db::models::{ReleaseAsset, ReleaseRow};
use crate::backend::files::{DownloadError, UploadError};
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::refs::RefError;
use crate::backend::repositories::releases::{
    self as releases, ReleaseError, ReleaseFields, ReleaseInput,
};
use crate::backend::repositories::store::ErrorKind;
use crate::webui::app::Page;
use crate::webui::pages::repos::chrome::RepoChrome;
use crate::webui::pages::repos::page::Page as RepoPage;
use crate::webui::pages::repos::releases::{
    AssetItem, Page as ReleasePage, ReleaseDraft, ReleaseItem,
};
use axum::extract::{Form, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::Arc;

const PER_PAGE: i64 = 30;

fn release_item(row: &ReleaseRow) -> ReleaseItem {
    let release = &row.release;
    ReleaseItem {
        id: release.id,
        tag: release.tag_name.clone(),
        title: release
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        body: release.body.clone().unwrap_or_default(),
        author: row.author.clone(),
        target: release.target_commitish.clone(),
        created_at: release.created_at.to_rfc3339(),
        published_at: release.published_at.map(|date| date.to_rfc3339()),
        draft: release.is_draft,
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
            crate::webui::encode_component(tag),
            crate::webui::encode_component(&asset.name)
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

fn field(form: &HashMap<String, String>, name: &str) -> String {
    form.get(name).cloned().unwrap_or_default()
}

/// The release form as submitted. A checkbox is in the form only when it is ticked.
fn submitted_draft(form: &HashMap<String, String>) -> ReleaseDraft {
    ReleaseDraft {
        tag: field(form, "tag"),
        target: field(form, "target"),
        title: field(form, "title"),
        body: field(form, "body"),
        prerelease: form.contains_key("prerelease"),
        draft: form.contains_key("draft"),
        latest: form.contains_key("latest"),
        preview: field(form, "intent") == "preview",
    }
}

fn fields_of(draft: &ReleaseDraft) -> ReleaseFields {
    let title = draft.title.trim();
    ReleaseFields {
        name: (!title.is_empty()).then(|| title.to_owned()),
        body: (!draft.body.trim().is_empty()).then(|| draft.body.clone()),
        is_draft: draft.draft,
        is_prerelease: draft.prerelease,
        make_latest: draft.latest,
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
    let choices = match releases::form_choices(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        actor,
        &repo.owner,
        &repo.name,
    )
    .await
    {
        Ok(choices) => choices,
        Err(error) => return refusal(ctx, repo, error),
    };
    ctx.page_model(
        "New release",
        repo_page(
            repo.clone(),
            ReleasePage::Form {
                editing: None,
                draft,
                tags: choices.tags,
                branches: choices.branches,
                default_branch: choices.default_branch,
                assets: Vec::new(),
                error,
            },
        ),
    )
}

/// The edit form of a release, with the assets as they are stored. A refused edit passes the
/// draft the writer typed, so nothing they entered is lost.
async fn edit_form(
    ctx: &PageContext,
    repo: &RepoChrome,
    owner: &str,
    name: &str,
    id: i64,
    draft: Option<ReleaseDraft>,
    error: Option<String>,
) -> Response {
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to edit a release.");
    };
    let found = match releases::edit_detail(&ctx.state.pool, actor, owner, name, id).await {
        Ok(found) => found,
        Err(error) => return refusal(ctx, repo, error),
    };
    let base = repo.base();
    let tag = found.release.release.tag_name.clone();
    let draft = draft.unwrap_or_else(|| ReleaseDraft {
        tag: tag.clone(),
        target: found.release.release.target_commitish.clone(),
        title: found.release.release.name.clone().unwrap_or_default(),
        body: found.release.release.body.clone().unwrap_or_default(),
        prerelease: found.release.release.is_prerelease,
        draft: found.release.release.is_draft,
        latest: found.release.release.is_latest,
        preview: false,
    });
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match releases::list(&ctx.state.pool, viewer, &owner, &name, page, PER_PAGE).await {
        Ok(listing) => {
            let items = listing.releases.iter().map(release_item).collect();
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match releases::detail_by_tag(&ctx.state.pool, viewer, &owner, &name, &tag).await {
        Ok(found) => {
            let base = repo.base();
            let release = release_item(&found.release);
            let assets = found
                .assets
                .iter()
                .map(|asset| asset_item(&base, &release.tag, asset))
                .collect();
            let title = release.title.clone();
            ctx.page_model(
                &title,
                repo_page(
                    repo,
                    ReleasePage::Detail {
                        release,
                        assets,
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let draft = submitted_draft(&form);
    if draft.preview {
        return new_form(&ctx, &repo, draft, None).await;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to publish a release.");
    };
    let input = ReleaseInput {
        tag: draft.tag.trim().to_owned(),
        target: draft.target.trim().to_owned(),
        fields: fields_of(&draft),
    };
    match releases::create(
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
            crate::webui::encode_component(&release.tag_name)
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let draft = submitted_draft(&form);
    if draft.preview {
        return edit_form(&ctx, &repo, &owner, &name, id, Some(draft), None).await;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to edit a release.");
    };
    match releases::update(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        actor,
        &owner,
        &name,
        id,
        &fields_of(&draft),
    )
    .await
    {
        Ok(release) => ctx.redirect(&format!(
            "{}/releases/tag/{}",
            repo.base(),
            crate::webui::encode_component(&release.tag_name)
        )),
        Err(error) => {
            let (status, message) = refused(&error);
            with_status(
                edit_form(&ctx, &repo, &owner, &name, id, Some(draft), Some(message)).await,
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to delete a release.");
    };
    match releases::delete(
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
    // The body is read before the CSRF field can be checked, so only signed-in users get that far.
    if ctx.user.is_none() {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to upload an asset.");
    }
    let mut fields = HashMap::new();
    let mut file = None;
    loop {
        let part = match multipart.next_field().await {
            Ok(Some(part)) => part,
            Ok(None) => break,
            Err(_) => return ctx.error(StatusCode::BAD_REQUEST, "The upload could not be read."),
        };
        let part_name = part.name().unwrap_or_default().to_owned();
        if part_name == "file" {
            let filename = part.file_name().unwrap_or_default().to_owned();
            match part.bytes().await {
                Ok(bytes) => file = Some((filename, bytes.to_vec())),
                Err(_) => {
                    return ctx.error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Assets are limited to 100 MiB.",
                    )
                }
            }
        } else if let Ok(text) = part.text().await {
            fields.insert(part_name, text);
        }
    }
    if let Err(response) = authorize_post(&ctx, &fields) {
        return response;
    }
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
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
    match releases::add_asset(
        &ctx.state.pool,
        &ctx.state.storage,
        actor,
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
    let repo = match chrome::load_chrome(&ctx, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to remove an asset.");
    };
    match releases::delete_asset(
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
    Path((owner, name, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let Some((tag, asset)) = rest.rsplit_once('/') else {
        return ctx.error(StatusCode::NOT_FOUND, "Asset not found.");
    };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match releases::download(
        &ctx.state.pool,
        &ctx.state.storage,
        viewer,
        &owner,
        &name,
        tag,
        asset,
    )
    .await
    {
        Ok(opened) => match asset_response(opened) {
            Ok(response) => response,
            Err(failure) => failure.into_response(),
        },
        Err(error) => match chrome::load_chrome(&ctx, &owner, &name).await {
            Ok(repo) => refusal(&ctx, &repo, error),
            Err(response) => response,
        },
    }
}
