//! Upload files: a browser form that adds several files to one branch in one commit. The page
//! and its form share the address `/OWNER/REPO/upload/REF` or `/OWNER/REPO/upload/REF/DIR`.
use crate::chrome::resolve;
use crate::code::contents::write_status;
use axum::extract::multipart::Multipart;
use axum::extract::multipart::MultipartError;
use axum::extract::multipart::MultipartRejection;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_repos::editor::UploadFile;
use codeyard_repos::editor::UploadWrite;
use codeyard_repos::editor::MAX_UPLOAD_FILES;
use codeyard_repos::editor::MAX_UPLOAD_FILE_BYTES;
use codeyard_repos::editor::MAX_UPLOAD_TOTAL_BYTES;
use codeyard_webui::pages::repos as view;
use codeyard_webui::pages::repos::ref_path::branch_url;
use codeyard_webui::pages::repos::ref_path::compare_url;
use std::collections::HashMap;
use std::sync::Arc;

/// The most a request may carry: every file plus room for the commit message and the
/// multipart framing.
pub const BODY_LIMIT: usize = MAX_UPLOAD_TOTAL_BYTES + 64 * 1024;
/// Text fields are the commit message, its description and the form's own hidden fields.
const MAX_FIELDS: usize = 9;
const MAX_FIELD_BYTES: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum UploadFormError {
    FileTooLarge,
    UploadTooLarge,
    TooManyFiles,
    Unreadable,
    TimedOut,
}

impl UploadFormError {
    fn status(&self) -> StatusCode {
        match self {
            Self::FileTooLarge | Self::UploadTooLarge | Self::TooManyFiles => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::Unreadable => StatusCode::BAD_REQUEST,
            Self::TimedOut => StatusCode::REQUEST_TIMEOUT,
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::FileTooLarge => "Each file can be at most 25 MiB.",
            Self::UploadTooLarge => "An upload can be at most 100 MiB in total.",
            Self::TooManyFiles => "An upload can have at most 100 files.",
            Self::Unreadable => "The upload could not be read. Choose the files again.",
            Self::TimedOut => "The upload took too long to arrive. Choose the files again.",
        }
    }
}

fn read_error(error: MultipartError) -> UploadFormError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        UploadFormError::UploadTooLarge
    } else {
        UploadFormError::Unreadable
    }
}

/// The text fields, and the name and size of each file. The files' bytes sit in `content`, one
/// after another in the order of `files`.
struct UploadParts {
    fields: HashMap<String, String>,
    files: Vec<(String, usize)>,
    content: Vec<u8>,
}

/// Reads the whole form. It stops as soon as a file or the whole upload is larger than allowed,
/// or the form is not complete within `READ_TIMEOUT`.
async fn read(multipart: Multipart) -> Result<UploadParts, UploadFormError> {
    tokio::time::timeout(READ_TIMEOUT, read_parts(multipart))
        .await
        .unwrap_or(Err(UploadFormError::TimedOut))
}

async fn read_parts(mut multipart: Multipart) -> Result<UploadParts, UploadFormError> {
    let mut form = UploadParts {
        fields: HashMap::new(),
        files: Vec::new(),
        content: Vec::new(),
    };
    let mut parts = 0;
    while let Some(mut part) = multipart.next_field().await.map_err(read_error)? {
        parts += 1;
        if parts > MAX_FIELDS + MAX_UPLOAD_FILES {
            return Err(UploadFormError::Unreadable);
        }
        let name = part.name().unwrap_or_default().to_owned();
        if name == "files" {
            let file_name = part.file_name().unwrap_or_default().to_owned();
            let start = form.content.len();
            while let Some(chunk) = part.chunk().await.map_err(read_error)? {
                if form.content.len() - start + chunk.len() > MAX_UPLOAD_FILE_BYTES {
                    return Err(UploadFormError::FileTooLarge);
                }
                if form.content.len() + chunk.len() > MAX_UPLOAD_TOTAL_BYTES {
                    return Err(UploadFormError::UploadTooLarge);
                }
                form.content.extend_from_slice(&chunk);
            }
            // An empty file input sends a part with no name and no bytes. It is not a file.
            if file_name.is_empty() {
                form.content.truncate(start);
                continue;
            }
            if form.files.len() == MAX_UPLOAD_FILES {
                return Err(UploadFormError::TooManyFiles);
            }
            form.files.push((file_name, form.content.len() - start));
        } else {
            let value = codeyard_api_http::multipart::bounded_text(&mut part, MAX_FIELD_BYTES)
                .await
                .map_err(read_error)?
                .ok_or(UploadFormError::Unreadable)?;
            form.fields.insert(name, value);
        }
    }
    Ok(form)
}

/// The form for the branch and directory the URL names. Its message and description are empty,
/// and the revision is the branch's tip as the page is opened.
async fn target(
    ctx: &PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    tail: &str,
) -> Result<view::editor::UploadForm, Response> {
    let resolved = resolve(ctx, loaded, tail, false)?;
    let branch = crate::chrome::branch_name(&resolved.reference).to_owned();
    let reference = format!("refs/heads/{branch}");
    if !loaded.refs.is_empty() && !crate::chrome::is_branch(loaded, &resolved.reference) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose a branch, not a tag or detached commit, to upload files.",
        ));
    }
    let mut form = view::editor::UploadForm {
        repository_id: loaded.repository.id,
        expected_revision: loaded
            .refs
            .get(&reference)
            .cloned()
            .unwrap_or_else(|| "unborn".into()),
        branch,
        directory: resolved.path,
        ..Default::default()
    };
    if let Some(user) = ctx.user.as_ref() {
        form.protected = codeyard_repos_pulls::repositories::files::direct_commit_refused(
            &ctx.state.pool,
            &loaded.repository,
            user,
            &form.branch,
            &form.expected_revision,
        )
        .await;
        form.new_branch =
            codeyard_repos_pulls::repositories::files::free_branch_name(&user.username, &loaded.refs);
    }
    Ok(form)
}

/// The upload page again, with what went wrong and the text the user typed.
fn refusal(
    ctx: &PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    mut form: view::editor::UploadForm,
    status: StatusCode,
    message: &str,
) -> Response {
    form.error = Some(message.into());
    with_status(upload_page(ctx, loaded, form), status)
}

fn upload_page(
    ctx: &PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    form: view::editor::UploadForm,
) -> Response {
    ctx.page_model(
        "Upload files",
        codeyard_webui::app::Page::Repositories(view::page::Page::Upload {
            repo: loaded.chrome.clone(),
            form,
        }),
    )
}

fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

pub(crate) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    crate::code::GitPathParams(params): crate::code::GitPathParams,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match crate::chrome::load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if !loaded.chrome.can_write {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match target(&ctx, &loaded, tail).await {
        Ok(form) => upload_page(&ctx, &loaded, form),
        Err(response) => response,
    }
}

pub(crate) async fn post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    crate::code::GitPathParams(params): crate::code::GitPathParams,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match crate::chrome::load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if !loaded.chrome.can_write {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    let mut form = match target(&ctx, &loaded, tail).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    // Each upload is held in memory until its commit lands, so only a few run at once. The slot
    // is taken before the body is read, so a busy server reads nothing from the request.
    let Ok(_slot) = ctx.state.upload_reads.clone().try_acquire_owned() else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Uploads are busy. Try again in a moment.",
        );
    };
    let Ok(multipart) = multipart else {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "The upload could not be read. Open the upload page again.",
        );
    };
    let parts = match read(multipart).await {
        Ok(parts) => parts,
        Err(error) => return ctx.error(error.status(), error.message()),
    };
    // Multipart bodies may take minutes. Authenticate the original cookie and its current
    // synchronizer again, rather than retaining the authority cached before the upload.
    let ctx = match PageContext::load(ctx.state.clone(), ctx.headers.clone(), &uri).await {
        Ok(current)
            if current.user.as_ref().map(|user| user.id)
                == ctx.user.as_ref().map(|user| user.id)
                && current.user.is_some() =>
        {
            current
        }
        Ok(_) => {
            return ctx.error(
                StatusCode::UNAUTHORIZED,
                "Your session expired. Sign in again.",
            )
        }
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&parts.fields) {
        return response;
    }
    form.message = field(&parts.fields, "message");
    form.description = field(&parts.fields, "description");
    form.expected_revision = field(&parts.fields, "expected_revision");
    form.new_branch = field(&parts.fields, "new_branch");
    form.commit_to_new_branch = field(&parts.fields, "commit_target") == "new_branch";
    form.repository_id = field(&parts.fields, "repository_id").parse().unwrap_or(0);
    if form.repository_id != loaded.repository.id {
        return refusal(
            &ctx,
            &loaded,
            form,
            StatusCode::CONFLICT,
            "This repository was replaced after the upload form opened. Nothing was uploaded.",
        );
    }
    // The action path splits into a branch and a directory by the current refs, which can change.
    // A form rendered before this field existed has no branch to compare.
    let opened = field(&parts.fields, "branch");
    if !opened.is_empty() && opened != form.branch {
        return refusal(
            &ctx,
            &loaded,
            form,
            StatusCode::CONFLICT,
            "The branch changed after the upload form opened. Nothing was uploaded.",
        );
    }
    // The choice of a protected branch: the files land on a new branch, which starts there.
    let new_branch =
        (field(&parts.fields, "commit_target") == "new_branch").then(|| form.new_branch.clone());
    if parts.files.is_empty() {
        return refusal(
            &ctx,
            &loaded,
            form,
            StatusCode::BAD_REQUEST,
            "Choose at least one file to upload.",
        );
    }
    let mut message = if form.message.trim().is_empty() {
        view::editor::UPLOAD_MESSAGE.to_owned()
    } else {
        form.message.clone()
    };
    if !form.description.trim().is_empty() {
        message = format!("{message}\n\n{}", form.description.trim_end());
    }
    // A browser may send a path with directories in it. Only the file name is kept.
    let files = parts
        .files
        .iter()
        .map(|(name, size)| {
            let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
            UploadFile {
                path: join(&form.directory, name),
                size: *size,
            }
        })
        .collect();
    let write = UploadWrite {
        branch: form.branch.clone(),
        message,
        expected_revision: form.expected_revision.clone(),
        files,
    };
    let actor = ctx.database_actor(ctx.user.as_ref().expect("authenticated upload").id);
    let saved = codeyard_repos_pulls::repositories::files::upload_files(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.ci_runs,
        actor,
        &loaded.chrome.owner,
        loaded.repository.id,
        write,
        parts.content,
        new_branch.as_deref(),
    )
    .await;
    match saved {
        Ok(_) => ctx.redirect(&match &new_branch {
            Some(name) if form.expected_revision != "unborn" => {
                compare_url(&loaded.chrome.base(), &form.branch, name)
            }
            Some(name) => branch_url(&loaded.chrome.base(), "tree", name, &form.directory),
            None => branch_url(&loaded.chrome.base(), "tree", &form.branch, &form.directory),
        }),
        Err(error) => {
            form.saved =
                error.kind == codeyard_repos_pulls::repositories::files::WriteFailure::SavedFollowupFailed;
            refusal(&ctx, &loaded, form, write_status(&error), &error.message)
        }
    }
}

use codeyard_api_http::shared::repos::code::upload::READ_TIMEOUT;
