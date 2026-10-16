use super::repository::{field, with_status};
use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::chrome::{resolve, store_error, Loaded};
use crate::backend::api::repos::references::{resolve_references, resolve_references_or_none};
use crate::backend::api::repos::responses::{AuthorInfo, CommitResponse};
use crate::backend::app::AppState;
use crate::backend::repositories::files;
use crate::backend::repositories::store::{repo_store, FileWrite, MAX_EDITOR_BYTES};
use crate::webui::components::references::References;
use crate::webui::pages::repositories as view;
use crate::webui::pages::repositories::ref_path::{compare_url, file_url, is_markdown_path};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
#[derive(Deserialize)]
pub struct UpdateFileRequest {
    pub content: String,
    pub message: String,
    pub branch: String,
    /// Complete branch OID, not a blob SHA. Missing preconditions are rejected.
    pub expected_revision: String,
}

#[derive(Serialize)]
pub struct UpdateFileResponse {
    pub commit: CommitResponse,
}

#[derive(Deserialize)]
pub struct CreateFileRequest {
    pub content: String,
    pub message: String,
    pub branch: String,
    /// Complete branch OID, or `unborn` for an absent branch.
    pub expected_revision: String,
}

#[derive(Deserialize)]
pub struct DeleteFileRequest {
    pub message: String,
    pub branch: String,
    /// Complete branch OID, as for a save. Missing preconditions are rejected.
    pub expected_revision: String,
}

#[derive(Serialize)]
pub struct CreateFileResponse {
    pub sha: String,
    pub message: String,
    pub path: String,
}

fn commit_response(saved: files::FileSave) -> Json<UpdateFileResponse> {
    let user = saved.actor;
    let author = AuthorInfo {
        id: Some(user.id),
        username: Some(user.username),
        avatar_url: user
            .avatar_file_id
            .map(|id| format!("/api/files/{id}/download")),
    };
    Json(UpdateFileResponse {
        commit: CommitResponse::from_commit(saved.commit, Some(author)),
    })
}

type Failure = (StatusCode, Json<ApiError>);

fn failure(status: StatusCode, code: &str, message: &str) -> Failure {
    (status, Json(ApiError::new(code, message)))
}

fn decode_content(content: &str) -> Result<Vec<u8>, Failure> {
    if content.len() > MAX_EDITOR_BYTES.div_ceil(3) * 4 {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "File writes support at most 1 MiB.",
        ));
    }
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, content)
        .map_err(|_| {
            failure(
                StatusCode::BAD_REQUEST,
                "validation_error",
                "Invalid base64 content.",
            )
        })?;
    if bytes.len() > MAX_EDITOR_BYTES {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "File writes support at most 1 MiB.",
        ));
    }
    Ok(bytes)
}

pub async fn update_file_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, path)): Path<(String, String, String)>,
    Json(req): Json<UpdateFileRequest>,
) -> Result<Json<UpdateFileResponse>, Failure> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    files::ensure_readable(&state.pool, &actor, &owner, &repo)
        .await
        .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    let content = decode_content(&req.content)?;
    let saved = files::write_file_bytes(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        &repo,
        FileWrite {
            branch: req.branch,
            path,
            message: req.message,
            expected_revision: req.expected_revision,
            creating: false,
            deleting: false,
        },
        content,
    )
    .await
    .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    Ok(commit_response(saved))
}

/// Removes a file in one commit. The branch must still be at `expected_revision`, as for a save.
pub async fn delete_file_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, path)): Path<(String, String, String)>,
    Json(req): Json<DeleteFileRequest>,
) -> Result<Json<UpdateFileResponse>, Failure> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    let repository = files::named_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    let saved = files::delete_file(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        repository.id,
        FileWrite {
            branch: req.branch,
            path,
            message: req.message,
            expected_revision: req.expected_revision,
            creating: false,
            deleting: true,
        },
        None,
    )
    .await
    .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    Ok(commit_response(saved))
}

pub async fn create_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, path)): Path<(String, String, String)>,
    Json(req): Json<CreateFileRequest>,
) -> Result<(StatusCode, Json<CreateFileResponse>), Failure> {
    let actor = crate::backend::api::session::get_current_user(&state.pool, &headers).await?;
    files::ensure_readable(&state.pool, &actor, &owner, &repo)
        .await
        .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    let content = decode_content(&req.content)?;
    let saved = files::write_file_bytes(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor.id,
        &owner,
        &repo,
        FileWrite {
            branch: req.branch,
            path: path.clone(),
            message: req.message,
            expected_revision: req.expected_revision,
            creating: true,
            deleting: false,
        },
        content,
    )
    .await
    .map_err(|error| failure(write_status(&error), "file_write_rejected", &error.message))?;
    Ok((
        StatusCode::CREATED,
        Json(CreateFileResponse {
            sha: saved.commit.sha,
            message: saved.commit.message,
            path,
        }),
    ))
}

/// The branch and path a write page works on, and the branch's tip that its commit must build on.
/// Only a branch takes a commit, so a tag or a detached commit is refused.
#[expect(
    clippy::result_large_err,
    reason = "The page's error response is returned as is, like the other page helpers."
)]
fn write_target(
    ctx: &PageContext,
    loaded: &Loaded,
    tail: &str,
    require_path: bool,
) -> Result<(String, String, String), Response> {
    if !loaded.chrome.can_write {
        return Err(ctx.error(StatusCode::FORBIDDEN, "Write access is required."));
    }
    let resolved = resolve(ctx, loaded, tail, require_path)?;
    if !loaded.refs.is_empty()
        && !loaded
            .refs
            .contains_key(&format!("refs/heads/{}", resolved.reference))
    {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose a branch, not a tag or detached commit, to edit files.",
        ));
    }
    let expected = loaded
        .refs
        .get(&format!("refs/heads/{}", resolved.reference))
        .cloned()
        .unwrap_or_else(|| "unborn".into());
    Ok((resolved.reference, resolved.path, expected))
}

/// The name a commit on a new branch gets when the branch refuses a direct commit to this user.
fn new_branch_for(ctx: &PageContext, loaded: &Loaded, protected: bool) -> String {
    match (protected, ctx.user.as_ref()) {
        (true, Some(user)) => files::free_branch_name(&user.username, &loaded.refs),
        _ => String::new(),
    }
}

async fn editor_page(
    ctx: &PageContext,
    loaded: &Loaded,
    tail: &str,
    creating: bool,
) -> Result<Response, Response> {
    ctx.require_user()?;
    let (branch, path, expected) = write_target(ctx, loaded, tail, !creating)?;
    let mut editor = view::Editor {
        path: if creating && !path.is_empty() {
            format!("{path}/")
        } else {
            path.clone()
        },
        branch,
        creating,
        expected_revision: expected,
        ..Default::default()
    };
    if !creating {
        let blob = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
            .blob_bytes(&editor.expected_revision, &path)
            .await
            .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
        editor.content = files::editor_text(&blob.bytes, blob.is_binary)
            .map_err(|error| ctx.error(write_status(&error), &error.message))?
            .to_owned();
        editor.message = format!("Update {}", editor.path);
    }
    editor.references =
        editor_references(ctx, loaded, &editor.path, &editor.content, creating).await?;
    editor.protected =
        refuses_direct_commit(ctx, loaded, &editor.branch, &editor.expected_revision).await;
    editor.new_branch = new_branch_for(ctx, loaded, editor.protected);
    Ok(ctx.page_model(
        if creating { "New file" } else { "Edit file" },
        crate::webui::app::Page::Repositories(view::Page::Editor {
            repo: loaded.chrome.clone(),
            editor,
        }),
    ))
}

/// The references the editor's text links, for its preview. When the editor opens, the text is the
/// file as stored; after a preview or a refused save it is the submitted text, so the preview
/// links what it shows. An existing file resolves them only when it is Markdown, and any other
/// file links nothing. A new file always resolves them, because the path typed into it may become
/// Markdown after the page loads.
async fn editor_references(
    ctx: &PageContext,
    loaded: &Loaded,
    path: &str,
    text: &str,
    creating: bool,
) -> Result<References, Response> {
    if !creating && !is_markdown_path(path) {
        return Ok(References::default());
    }
    resolve_references(
        ctx,
        &loaded.repository,
        &loaded.chrome.owner,
        &loaded.chrome.name,
        &[text],
        // The editor's preview renders the file as a document.
        true,
    )
    .await
}

/// The references for a page that shows the user's submitted text again, a preview or a refused
/// save. That text must survive the page, so a failed lookup links nothing instead of failing it.
async fn editor_references_or_none(
    ctx: &PageContext,
    loaded: &Loaded,
    path: &str,
    text: &str,
    creating: bool,
) -> References {
    if !creating && !is_markdown_path(path) {
        return References::default();
    }
    resolve_references_or_none(
        ctx,
        &loaded.repository,
        &loaded.chrome.owner,
        &loaded.chrome.name,
        &[text],
        // The editor's preview renders the file as a document.
        true,
    )
    .await
}

pub(super) fn editor_from(form: &HashMap<String, String>) -> view::Editor {
    view::Editor {
        branch: field(form, "branch"),
        path: field(form, "path"),
        content: field(form, "content"),
        message: field(form, "message"),
        expected_revision: field(form, "expected_revision"),
        creating: field(form, "creating") == "yes",
        preview: field(form, "intent") == "preview",
        error: None,
        saved: false,
        references: Default::default(),
        protected: false,
        new_branch: field(form, "new_branch"),
    }
}

/// Whether the branch at `revision` refuses this user a direct commit, so the page offers a new branch.
async fn refuses_direct_commit(
    ctx: &PageContext,
    loaded: &Loaded,
    branch: &str,
    revision: &str,
) -> bool {
    match ctx.user.as_ref() {
        Some(user) => {
            files::direct_commit_refused(
                &ctx.state.pool,
                &loaded.repository,
                user,
                branch,
                revision,
            )
            .await
        }
        None => false,
    }
}

// A form is POST-only, so the appearance switch on an editor page returns to the editor's
// own GET page instead.
fn return_to_editor(ctx: &mut PageContext, loaded: &Loaded, editor: &view::Editor) {
    ctx.return_to = Some(file_url(
        &loaded.chrome.base(),
        if editor.creating { "new" } else { "edit" },
        &editor.branch,
        &editor.path,
    ));
}

pub(super) async fn write_form(
    mut ctx: PageContext,
    loaded: &Loaded,
    form: &HashMap<String, String>,
) -> Response {
    let mut editor = editor_from(form);
    return_to_editor(&mut ctx, loaded, &editor);
    if !loaded.chrome.can_write {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    if editor.preview {
        editor.references =
            editor_references_or_none(&ctx, loaded, &editor.path, &editor.content, editor.creating)
                .await;
        editor.protected =
            refuses_direct_commit(&ctx, loaded, &editor.branch, &editor.expected_revision).await;
        return ctx.page_model(
            "Preview file",
            crate::webui::app::Page::Repositories(view::Page::Editor {
                repo: loaded.chrome.clone(),
                editor,
            }),
        );
    }
    if field(form, "intent") != "save" {
        return ctx.error(StatusCode::BAD_REQUEST, "Choose save or preview.");
    }
    let input = FileWrite {
        branch: editor.branch.clone(),
        path: editor.path.clone(),
        message: editor.message.clone(),
        expected_revision: editor.expected_revision.clone(),
        creating: editor.creating,
        deleting: false,
    };
    // The choice of a protected branch: the commit lands on a new branch, which starts there.
    let new_branch =
        (field(form, "commit_target") == "new_branch").then(|| editor.new_branch.clone());
    match files::write_file(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.ci_runs,
        ctx.user
            .as_ref()
            .expect("authenticated editor submission")
            .id,
        &loaded.chrome.owner,
        loaded.repository.id,
        input,
        editor.content.clone(),
        new_branch.as_deref(),
    )
    .await
    {
        Ok(_) => ctx.redirect(&match &new_branch {
            Some(name) => compare_url(&loaded.chrome.base(), &editor.branch, name),
            None => file_url(&loaded.chrome.base(), "blob", &editor.branch, &editor.path),
        }),
        Err(error) => {
            let status = write_status(&error);
            editor.saved = error.kind == files::WriteFailure::SavedFollowupFailed;
            let title = if editor.saved {
                view::SAVED_FOLLOWUP_FAILED
            } else {
                "File not saved"
            };
            editor.error = Some(error.message);
            editor.protected =
                refuses_direct_commit(&ctx, loaded, &editor.branch, &editor.expected_revision)
                    .await;
            editor.references = editor_references_or_none(
                &ctx,
                loaded,
                &editor.path,
                &editor.content,
                editor.creating,
            )
            .await;
            with_status(
                ctx.page_model(
                    title,
                    crate::webui::app::Page::Repositories(view::Page::Editor {
                        repo: loaded.chrome.clone(),
                        editor,
                    }),
                ),
                status,
            )
        }
    }
}

/// Refuses a save to a repository that was replaced at this address after the editor
/// opened. Nothing is written. The editor comes back with the submitted text, and its
/// hidden repository ID stays the one the form was rendered for, so submitting again is
/// refused again. An empty or invalid ID becomes 0, which no repository has.
pub(super) async fn replaced_repository_editor(
    mut ctx: PageContext,
    loaded: &Loaded,
    form: &HashMap<String, String>,
) -> Response {
    let mut editor = editor_from(form);
    return_to_editor(&mut ctx, loaded, &editor);
    editor.error = Some(
        "This repository was replaced after the editor opened, so nothing was saved. Your changes are kept below."
            .into(),
    );
    editor.references =
        editor_references_or_none(&ctx, loaded, &editor.path, &editor.content, editor.creating)
            .await;
    let mut repo = loaded.chrome.clone();
    repo.id = field(form, "repository_id").parse().unwrap_or(0);
    with_status(
        ctx.page_model(
            "File not saved",
            crate::webui::app::Page::Repositories(view::Page::Editor { repo, editor }),
        ),
        StatusCode::CONFLICT,
    )
}

/// The confirm page for removing a file. It takes the commit fields of a save, without the editor.
async fn delete_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    ctx.require_user()?;
    let (branch, path, expected) = write_target(ctx, loaded, tail, true)?;
    let protected = refuses_direct_commit(ctx, loaded, &branch, &expected).await;
    let file = view::DeleteFile {
        message: format!("Delete {path}"),
        new_branch: new_branch_for(ctx, loaded, protected),
        branch,
        path,
        expected_revision: expected,
        error: None,
        protected,
    };
    Ok(ctx.page_model(
        "Delete file",
        crate::webui::app::Page::Repositories(view::Page::DeleteFile {
            repo: loaded.chrome.clone(),
            file,
        }),
    ))
}

/// Saves a delete from the confirm page. A refused delete shows the page again with the commit
/// fields as they were typed.
pub(super) async fn delete_form(
    mut ctx: PageContext,
    loaded: &Loaded,
    form: &HashMap<String, String>,
) -> Response {
    let branch = field(form, "branch");
    let path = field(form, "path");
    ctx.return_to = Some(file_url(&loaded.chrome.base(), "delete", &branch, &path));
    if !loaded.chrome.can_write {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    let mut file = view::DeleteFile {
        message: field(form, "message"),
        expected_revision: field(form, "expected_revision"),
        new_branch: field(form, "new_branch"),
        branch: branch.clone(),
        path: path.clone(),
        error: None,
        protected: false,
    };
    // The choice of a protected branch: the commit lands on a new branch, which starts there.
    let new_branch =
        (field(form, "commit_target") == "new_branch").then(|| file.new_branch.clone());
    let input = FileWrite {
        branch: branch.clone(),
        path,
        message: file.message.clone(),
        expected_revision: file.expected_revision.clone(),
        creating: false,
        deleting: true,
    };
    match files::delete_file(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        &ctx.state.ci_runs,
        ctx.user
            .as_ref()
            .expect("authenticated delete submission")
            .id,
        &loaded.chrome.owner,
        loaded.repository.id,
        input,
        new_branch.as_deref(),
    )
    .await
    {
        Ok(_) => ctx.redirect(&match &new_branch {
            Some(name) => compare_url(&loaded.chrome.base(), &branch, name),
            None => file_url(&loaded.chrome.base(), "tree", &branch, ""),
        }),
        Err(error) => {
            let status = write_status(&error);
            let title = if error.kind == files::WriteFailure::SavedFollowupFailed {
                view::SAVED_FOLLOWUP_FAILED
            } else {
                view::DELETE_REFUSED
            };
            file.error = Some(error.message);
            file.protected =
                refuses_direct_commit(&ctx, loaded, &branch, &file.expected_revision).await;
            with_status(
                ctx.page_model(
                    title,
                    crate::webui::app::Page::Repositories(view::Page::DeleteFile {
                        repo: loaded.chrome.clone(),
                        file,
                    }),
                ),
                status,
            )
        }
    }
}

pub(super) fn write_status(error: &files::WriteError) -> StatusCode {
    use files::WriteFailure;
    match error.kind {
        WriteFailure::Invalid => StatusCode::BAD_REQUEST,
        WriteFailure::Conflict => StatusCode::CONFLICT,
        WriteFailure::Unavailable | WriteFailure::SavedFollowupFailed => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        WriteFailure::NotFound => StatusCode::NOT_FOUND,
        WriteFailure::Denied => StatusCode::FORBIDDEN,
        WriteFailure::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        WriteFailure::Encoding => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx = match crate::backend::api::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    let kind = uri.path().split('/').nth(3);
    let page = if kind == Some("delete") {
        delete_page(&ctx, &loaded, tail).await
    } else {
        editor_page(&ctx, &loaded, tail, kind == Some("new")).await
    };
    match page {
        Ok(response) | Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_requires_a_branch_precondition_and_bounds_binary_input() {
        let request = serde_json::json!({"branch":"main","content":"YQ==","message":"save"});
        assert!(serde_json::from_value::<CreateFileRequest>(request.clone()).is_err());
        assert!(serde_json::from_value::<UpdateFileRequest>(request).is_err());
        assert_eq!(decode_content("AP8=").unwrap(), vec![0, 255]);
        assert!(decode_content(&"a".repeat(MAX_EDITOR_BYTES.div_ceil(3) * 4 + 1)).is_err());
    }
}
