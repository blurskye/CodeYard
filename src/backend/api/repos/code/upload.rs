//! Upload files: a browser form that adds several files to one branch in one commit. The page
//! and its form share the address `/OWNER/REPO/upload/REF` or `/OWNER/REPO/upload/REF/DIR`.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome::{self, resolve};
use crate::backend::api::repos::code::contents::write_status;
use crate::backend::api::repos::repository::{field, with_status};
use crate::backend::app::AppState;
use crate::backend::repositories::files;
use crate::backend::repositories::store::{
    UploadFile, UploadWrite, MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES, MAX_UPLOAD_FILE_BYTES,
};
use crate::webui::pages::repos as view;
use crate::webui::pages::repos::ref_path::{compare_url, file_url};
use axum::{
    extract::{
        multipart::{Multipart, MultipartError, MultipartRejection},
        OriginalUri, Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// The route template, shared with the form-body middleware that exempts it.
pub const ROUTE: &str = "/:owner/:repo/upload/*ref_path";
/// The most a request may carry: every file plus room for the commit message and the
/// multipart framing.
pub const BODY_LIMIT: usize = MAX_UPLOAD_BYTES + 64 * 1024;
/// Text fields are the commit message, its description and the form's own hidden fields.
const MAX_FIELDS: usize = 8;
const MAX_FIELD_BYTES: usize = 16 * 1024;
/// How long an upload may take to arrive. Large files over a slow connection need minutes.
const READ_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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
                if form.content.len() + chunk.len() > MAX_UPLOAD_BYTES {
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
            let mut bytes = Vec::new();
            while let Some(chunk) = part.chunk().await.map_err(read_error)? {
                if bytes.len() + chunk.len() > MAX_FIELD_BYTES {
                    return Err(UploadFormError::Unreadable);
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = String::from_utf8(bytes).map_err(|_| UploadFormError::Unreadable)?;
            form.fields.insert(name, value);
        }
    }
    Ok(form)
}

/// The form for the branch and directory the URL names. Its message and description are empty,
/// and the revision is the branch's tip as the page is opened.
async fn target(
    ctx: &PageContext,
    loaded: &chrome::Loaded,
    tail: &str,
) -> Result<view::editor::UploadForm, Response> {
    let resolved = resolve(ctx, loaded, tail, false)?;
    let reference = format!("refs/heads/{}", resolved.reference);
    if !loaded.refs.is_empty() && !loaded.refs.contains_key(&reference) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose a branch, not a tag or detached commit, to upload files.",
        ));
    }
    let mut form = view::editor::UploadForm {
        expected_revision: loaded
            .refs
            .get(&reference)
            .cloned()
            .unwrap_or_else(|| "unborn".into()),
        branch: resolved.reference,
        directory: resolved.path,
        ..Default::default()
    };
    if let Some(user) = ctx.user.as_ref() {
        form.protected = files::direct_commit_refused(
            &ctx.state.pool,
            &loaded.repository,
            user,
            &form.branch,
            &form.expected_revision,
        )
        .await;
        form.new_branch = files::free_branch_name(&user.username, &loaded.refs);
    }
    Ok(form)
}

/// The upload page again, with what went wrong and the text the user typed.
fn refusal(
    ctx: &PageContext,
    loaded: &chrome::Loaded,
    mut form: view::editor::UploadForm,
    status: StatusCode,
    message: &str,
) -> Response {
    form.error = Some(message.into());
    with_status(upload_page(ctx, loaded, form), status)
}

fn upload_page(
    ctx: &PageContext,
    loaded: &chrome::Loaded,
    form: view::editor::UploadForm,
) -> Response {
    ctx.page_model(
        "Upload files",
        crate::webui::app::Page::Repositories(view::page::Page::Upload {
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

pub(in crate::backend::api::repos) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load(&ctx, &params["owner"], &params["repo"]).await {
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

pub(in crate::backend::api::repos) async fn post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load(&ctx, &params["owner"], &params["repo"]).await {
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
        return refusal(
            &ctx,
            &loaded,
            form,
            StatusCode::SERVICE_UNAVAILABLE,
            "Uploads are busy. Try again in a moment.",
        );
    };
    let Ok(multipart) = multipart else {
        return refusal(
            &ctx,
            &loaded,
            form,
            StatusCode::BAD_REQUEST,
            "The upload could not be read. Choose the files again.",
        );
    };
    let parts = match read(multipart).await {
        Ok(parts) => parts,
        Err(error) => return refusal(&ctx, &loaded, form, error.status(), error.message()),
    };
    if let Err(response) = ctx.check_csrf(&parts.fields) {
        return response;
    }
    form.message = field(&parts.fields, "message");
    form.description = field(&parts.fields, "description");
    form.expected_revision = field(&parts.fields, "expected_revision");
    form.new_branch = field(&parts.fields, "new_branch");
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
    let actor = ctx.user.as_ref().expect("authenticated upload").id;
    let saved = files::upload_files(
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
            Some(name) => compare_url(&loaded.chrome.base(), &form.branch, name),
            None => file_url(&loaded.chrome.base(), "tree", &form.branch, &form.directory),
        }),
        Err(error) => refusal(&ctx, &loaded, form, write_status(&error), &error.message),
    }
}
