use super::*;
use crate::backend::repositories::{
    files::native_text,
    store::{GistEdit, GistFileChange},
};
use std::collections::{btree_map::Entry, BTreeMap};

async fn protected(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    form: &Fields,
) -> Result<PageContext, Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    mutation_guard(&ctx, form, false)?;
    for key in ["comments_page", "history_page", "forks_page"] {
        if let Err(failure) = number(&ctx, key) {
            return Err(error(&ctx, failure));
        }
    }
    if form.get("body").is_some_and(|body| body.len() > 4 * 65535) {
        return Err(ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Comment exceeds the editor's size limit.",
        ));
    }
    Ok(ctx)
}
fn gist_context(ctx: &mut PageContext, id: &str) {
    let path = format!("/gists/{id}");
    ctx.return_to = Some(path.clone());
    ctx.path = path;
}
fn submitted(
    ctx: &PageContext,
    id: Option<String>,
    public: bool,
    form: &Fields,
) -> anyhow::Result<view::Editor> {
    let count = field(form, "file_count")
        .parse::<usize>()
        .ok()
        .filter(|count| (1..=200).contains(count))
        .ok_or(InvalidInput("Invalid file count. Reload the editor."))?;
    let description = native_text(field(form, "description").to_owned(), "");
    anyhow::ensure!(
        description.chars().count() <= 65535,
        InvalidInput("Description must be at most 65,535 characters.")
    );
    let mut files = Vec::new();
    let mut bytes = 0usize;
    for index in 0..count {
        let filename = field(form, &format!("name_{index}")).to_owned();
        let original = form
            .get(&format!("original_{index}"))
            .filter(|name| !name.is_empty())
            .cloned();
        let content = form.get(&format!("content_{index}")).cloned();
        bytes = bytes.saturating_add(content.as_ref().map_or(0, String::len));
        anyhow::ensure!(
            filename.len() <= 1024 && original.as_ref().is_none_or(|name| name.len() <= 1024),
            InvalidInput("Filename is too long.")
        );
        files.push(view::EditorFile {
            original,
            filename,
            content,
            delete: field(form, &format!("delete_{index}")) == "yes",
        });
    }
    let expected = field(form, "expected").to_owned();
    anyhow::ensure!(
        id.is_none()
            || (expected.len() == 40 && expected.bytes().all(|byte| byte.is_ascii_hexdigit())),
        InvalidInput("Reload the gist before editing; its revision is missing.")
    );
    let too_large = bytes > 2 * EDITOR_BYTES || files.iter().filter_map(|file| file.content.as_ref()).map(|content| content.replace("\r\n", "\n").len()).sum::<usize>() > EDITOR_BYTES;
    Ok(view::Editor {
        id,
        description,
        public,
        expected,
        files,
        csrf_token: ctx.csrf_token.clone(),
        // The draft goes back to the editor with this message.
        message: too_large.then(|| {
            "This editor accepts at most 1 MiB of text. Use Git for larger files.".to_owned()
        }),
    })
}
fn added_file(ctx: &PageContext, mut editor: view::Editor) -> Response {
    if editor.files.len() < 200 {
        editor.files.push(view::EditorFile {
            content: Some(String::new()),
            ..Default::default()
        });
    } else {
        editor.message = Some("Remove files before adding more rows.".into());
    }
    render_editor(ctx, StatusCode::OK, editor)
}
fn edit_input(editor: &view::Editor) -> anyhow::Result<GistEdit> {
    let mut files = BTreeMap::new();
    for file in &editor.files {
        if file.original.is_none()
            && file.filename.is_empty()
            && file.content.as_ref().is_none_or(String::is_empty)
        {
            continue;
        }
        let name = file.original.as_ref().unwrap_or(&file.filename).clone();
        let change = if file.delete {
            None
        } else {
            Some(GistFileChange {
                filename: Some(file.filename.clone()),
                content: file.content.clone(),
            })
        };
        match files.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(change);
            }
            Entry::Occupied(mut entry) => {
                if entry.get().is_none() && change.is_some() {
                    entry.insert(change);
                } else if !(entry.get().is_some() && change.is_none()) {
                    return Err(InvalidInput("Each filename must be unique.").into());
                }
            }
        }
    }
    Ok(GistEdit {
        description: Some(editor.description.clone()),
        files,
        expected: Some(editor.expected.clone()),
    })
}
pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let mut editor = match submitted(&ctx, None, field(&form, "public") == "yes", &form) {
        Ok(editor) if editor.message.is_some() => {
            return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor)
        }
        Ok(editor) => editor,
        Err(failure) => return error(&ctx, failure),
    };
    if field(&form, "intent") == "add-file" {
        return added_file(&ctx, editor);
    }
    let mut files = BTreeMap::new();
    for file in &editor.files {
        if file.delete
            || (file.filename.is_empty() && file.content.as_ref().is_none_or(String::is_empty))
        {
            continue;
        }
        let Some(content) = &file.content else {
            editor.message = Some("New files need text contents.".into());
            return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor);
        };
        // Browsers submit textareas with CRLF; new files are saved with LF.
        if files
            .insert(file.filename.clone(), native_text(content.clone(), ""))
            .is_some()
        {
            editor.message = Some("Each filename must be unique.".into());
            return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor);
        }
    }
    if files.values().map(String::len).sum::<usize>() > EDITOR_BYTES {
        editor.message = Some("This editor accepts at most 1 MiB of text. Use Git for larger files.".into());
        return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor);
    }
    match domain::create(
        ctx.state.clone(),
        ctx.user.as_ref().expect("protected account").id,
        credential(&ctx),
        domain::CreateGist {
            description: editor.description.clone(),
            is_public: editor.public,
            files,
        },
    )
    .await
    {
        Ok(loaded) => ctx.redirect(&format!("/gists/{}", loaded.gist.id)),
        Err(failure) => {
            let (status, message) = refusal(&failure);
            if let Some(saved) = failure.downcast_ref::<domain::GistSaved>() {
                editor.id = Some(saved.id.clone());
            }
            editor.message = Some(message);
            render_editor(&ctx, status, editor)
        }
    }
}
pub(super) async fn edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match domain::load(&ctx.state, &id, None).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    if ctx.user.as_ref().map(|user| user.id) != Some(loaded.gist.owner_id) {
        return error(&ctx, db::gists::GistRefusal::Denied.into());
    }
    let mut editor = match submitted(&ctx, Some(id.clone()), loaded.gist.is_public, &form) {
        Ok(editor) if editor.message.is_some() => {
            return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor)
        }
        Ok(editor) => editor,
        Err(failure) => return error(&ctx, failure),
    };
    if field(&form, "intent") == "add-file" {
        return added_file(&ctx, editor);
    }
    let mut input = match edit_input(&editor) {
        Ok(input) => input,
        Err(failure) => {
            let (status, message) = refusal(&failure);
            editor.message = Some(message);
            return render_editor(&ctx, status, editor);
        }
    };
    // Browsers submit textareas with CRLF; keep each file's own line breaks.
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    for (name, change) in input.files.iter_mut() {
        let Some(GistFileChange {
            content: Some(content),
            ..
        }) = change
        else {
            continue;
        };
        let original = match loaded
            .snapshot
            .files
            .iter()
            .find(|entry| entry.path == *name)
        {
            Some(entry) => match store.blob_by_id(&entry.sha).await {
                Ok(bytes) => bytes,
                Err(failure) => return error(&ctx, failure.into()),
            },
            None => Vec::new(),
        };
        *content = native_text(std::mem::take(content), &String::from_utf8_lossy(&original));
    }
    if input.files.values().filter_map(|change| change.as_ref().and_then(|change| change.content.as_ref())).map(String::len).sum::<usize>() > EDITOR_BYTES {
        editor.message = Some("This editor accepts at most 1 MiB of text. Use Git for larger files.".into());
        return render_editor(&ctx, StatusCode::UNPROCESSABLE_ENTITY, editor);
    }
    match domain::update(
        ctx.state.clone(),
        ctx.user.as_ref().expect("protected account").id,
        credential(&ctx),
        id.clone(),
        input,
    )
    .await
    {
        Ok(_) => ctx.redirect(&format!("/gists/{id}")),
        Err(failure) => {
            let (status, message) = refusal(&failure);
            editor.message = Some(message);
            render_editor(&ctx, status, editor)
        }
    }
}
struct CommentDraft {
    id: Option<i64>,
    body: String,
}
async fn detail_error(
    ctx: &PageContext,
    id: &str,
    failure: anyhow::Error,
    draft: Option<CommentDraft>,
) -> Response {
    let (status, message) = refusal(&failure);
    match pages::detail_model(ctx, id).await {
        Ok(mut page) => {
            page.message = Some(message);
            if let Some(draft) = draft {
                if let Some(id) = draft.id {
                    if let Some(comment) = page.comments.iter_mut().find(|comment| comment.id == id)
                    {
                        comment.edit_body = Some(draft.body);
                    }
                } else {
                    page.comment_body = draft.body;
                }
            }
            pages::detail_response(ctx, status, page)
        }
        Err(_) => ctx.error(status, &message),
    }
}
pub(super) async fn star(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    let loaded = match domain::load(&ctx.state, &id, None).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    match db::gists::star(
        &ctx.state.pool,
        ctx.user.as_ref().expect("protected account").id,
        &credential(&ctx).borrowed(),
        loaded.repository.id,
        field(&form, "add") == "yes",
    )
    .await
    {
        Ok(()) => ctx.redirect(&format!("/gists/{id}")),
        Err(failure) => detail_error(&ctx, &id, failure, None).await,
    }
}
pub(super) async fn fork(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    match domain::fork(
        ctx.state.clone(),
        ctx.user.as_ref().expect("protected account").id,
        credential(&ctx),
        id.clone(),
    )
    .await
    {
        Ok(loaded) => ctx.redirect(&format!("/gists/{}", loaded.gist.id)),
        Err(failure) => detail_error(&ctx, &id, failure, None).await,
    }
}
pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    match domain::delete(
        ctx.state.clone(),
        ctx.user.as_ref().expect("protected account").id,
        credential(&ctx),
        id,
    )
    .await
    {
        Ok(()) => ctx.redirect("/gists?tab=mine"),
        Err(failure) => error(&ctx, failure),
    }
}
pub(super) async fn comment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    let loaded = match domain::load(&ctx.state, &id, None).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    let body = field(&form, "body");
    match db::gists::create_comment(
        &ctx.state.pool,
        ctx.user.as_ref().expect("protected account").id,
        &credential(&ctx).borrowed(),
        loaded.repository.id,
        body,
    )
    .await
    {
        Ok(comment) => ctx.redirect(&format!(
            "/gists/{id}?comments_page={}#comment-{}",
            loaded.gist.comments as usize / PAGE_SIZE + 1,
            comment.id
        )),
        Err(failure) => {
            detail_error(
                &ctx,
                &id,
                failure,
                Some(CommentDraft {
                    id: None,
                    body: body.into(),
                }),
            )
            .await
        }
    }
}
pub(super) async fn edit_comment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, i64)>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    let loaded = match domain::load(&ctx.state, &id, None).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    let body = field(&form, "body");
    match db::gists::change_comment(
        &ctx.state.pool,
        ctx.user.as_ref().expect("protected account").id,
        &credential(&ctx).borrowed(),
        loaded.repository.id,
        comment,
        Some(body),
    )
    .await
    {
        Ok(_) => ctx.redirect(&format!(
            "/gists/{id}?comments_page={}#comment-{comment}",
            number(&ctx, "comments_page").expect("protected pagination")
        )),
        Err(failure) => {
            detail_error(
                &ctx,
                &id,
                failure,
                Some(CommentDraft {
                    id: Some(comment),
                    body: body.into(),
                }),
            )
            .await
        }
    }
}
pub(super) async fn delete_comment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, i64)>,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let mut ctx = match protected(state, headers, &uri, &form).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    gist_context(&mut ctx, &id);
    let loaded = match domain::load(&ctx.state, &id, None).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    match db::gists::change_comment(
        &ctx.state.pool,
        ctx.user.as_ref().expect("protected account").id,
        &credential(&ctx).borrowed(),
        loaded.repository.id,
        comment,
        None,
    )
    .await
    {
        Ok(_) => {
            let last = loaded.gist.comments.saturating_sub(2) as usize / PAGE_SIZE + 1;
            let page = number(&ctx, "comments_page")
                .expect("protected pagination")
                .min(last);
            ctx.redirect(&format!("/gists/{id}?comments_page={page}"))
        }
        Err(failure) => detail_error(&ctx, &id, failure, None).await,
    }
}
