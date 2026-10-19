//! Wiki pages and their forms. Readers see pages; writers create and edit them; administrators
//! delete pages and turn the wiki on or off. The database checks each write's authority with the
//! change, so these adapters only shape the pages and the refusals.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome;
use crate::backend::api::repos::pulls::pull::authorize_post;
use crate::backend::app::AppState;
use crate::backend::db::models::{WikiPage, WikiRevision};
use crate::backend::db::wiki::{self as wiki_db, WikiContent, WikiRefusal};
use crate::backend::repositories::wiki as rules;
use crate::webui::app::Page;
use crate::webui::pages::repos::chrome::RepoChrome;
use crate::webui::pages::repos::page::Page as RepoPage;
use crate::webui::pages::repos::wiki::{
    self as view, DiffLine, DraftFields, ListItem, Page as WikiPageView, PageItem, RevisionItem,
};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

fn repo_page(repo: RepoChrome, page: WikiPageView) -> Page {
    Page::Repositories(RepoPage::Wiki { repo, page })
}

fn page_item(page: &WikiPage) -> PageItem {
    PageItem {
        slug: page.slug.clone(),
        title: page.head.title.clone(),
        body: page.head.body.clone(),
        revision: page.head.number,
        author: page.head.author.clone(),
        saved_at: page.head.saved_at.to_rfc3339(),
        message: page.head.message.clone(),
    }
}

fn revision_item(revision: &WikiRevision) -> RevisionItem {
    RevisionItem {
        number: revision.number,
        title: revision.title.clone(),
        body: revision.body.clone(),
        message: revision.message.clone(),
        author: revision.author.clone(),
        saved_at: revision.saved_at.to_rfc3339(),
    }
}

fn field(form: &HashMap<String, String>, name: &str) -> String {
    form.get(name).cloned().unwrap_or_default()
}

fn draft_of(form: &HashMap<String, String>) -> DraftFields {
    DraftFields {
        title: field(form, "title"),
        body: field(form, "body"),
        message: field(form, "message"),
        preview: field(form, "intent") == "preview",
    }
}

/// Maps a refused write to the status and text the page shows. Refusals that a form can show
/// itself are handled by the callers.
fn refused(ctx: &PageContext, repo: &RepoChrome, error: anyhow::Error) -> Response {
    let (status, message) = match error.downcast_ref::<WikiRefusal>() {
        Some(WikiRefusal::Denied) => (
            StatusCode::FORBIDDEN,
            "You do not have permission to change this wiki.",
        ),
        Some(WikiRefusal::NotFound) => (StatusCode::NOT_FOUND, "Wiki page not found."),
        Some(WikiRefusal::SlugTaken) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "A page with this title already exists.",
        ),
        Some(WikiRefusal::Stale) => (
            StatusCode::CONFLICT,
            "The page changed while you were editing. Open it again and retry.",
        ),
        Some(WikiRefusal::PageLimit) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "This wiki has reached its page limit.",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Wiki storage is unavailable. Try again.",
        ),
    };
    ctx.repo_error(repo, status, message)
}

/// Pages are not served while the wiki is off.
#[expect(
    clippy::result_large_err,
    reason = "Adapters return the complete response for a refused request."
)]
fn enabled(ctx: &PageContext, repo: &RepoChrome) -> Result<(), Response> {
    if repo.wiki_enabled {
        Ok(())
    } else {
        Err(ctx.repo_error(repo, StatusCode::NOT_FOUND, "Wiki not found."))
    }
}

/// The signed-in account that may edit pages. The database checks the same rule with each change.
#[expect(
    clippy::result_large_err,
    reason = "Adapters return the complete response for a refused request."
)]
fn writer(ctx: &PageContext, repo: &RepoChrome) -> Result<i64, Response> {
    let Some(user) = ctx.user.as_ref() else {
        return Err(ctx.repo_error(
            repo,
            StatusCode::UNAUTHORIZED,
            "Sign in to change this wiki.",
        ));
    };
    if !repo.can_write {
        return Err(ctx.repo_error(
            repo,
            StatusCode::FORBIDDEN,
            "You do not have permission to change this wiki.",
        ));
    }
    Ok(user.id)
}

/// The signed-in account that may delete pages and turn the wiki on or off.
#[expect(
    clippy::result_large_err,
    reason = "Adapters return the complete response for a refused request."
)]
fn administrator(ctx: &PageContext, repo: &RepoChrome) -> Result<i64, Response> {
    let Some(user) = ctx.user.as_ref() else {
        return Err(ctx.repo_error(
            repo,
            StatusCode::UNAUTHORIZED,
            "Sign in to change this wiki.",
        ));
    };
    if !repo.can_admin {
        return Err(ctx.repo_error(
            repo,
            StatusCode::FORBIDDEN,
            "Only repository administrators can do that.",
        ));
    }
    Ok(user.id)
}

async fn load_page(ctx: &PageContext, repo: &RepoChrome, slug: &str) -> Result<WikiPage, Response> {
    match wiki_db::get_page_by_slug(&ctx.state.pool, repo.id, slug).await {
        Ok(Some(page)) => Ok(page),
        Ok(None) => Err(ctx.repo_error(repo, StatusCode::NOT_FOUND, "Wiki page not found.")),
        Err(error) => Err(refused(ctx, repo, error)),
    }
}

/// The page shown at `slug`, with the Sidebar and Footer pages and every address for the links.
async fn show_slug(ctx: &PageContext, repo: RepoChrome, slug: &str) -> Response {
    let pool = &ctx.state.pool;
    let found = match wiki_db::get_page_by_slug(pool, repo.id, slug).await {
        Ok(found) => found,
        Err(error) => return refused(ctx, &repo, error),
    };
    let Some(page) = found else {
        let target = match wiki_db::redirect_target(pool, repo.id, slug).await {
            Ok(target) => target,
            Err(error) => return refused(ctx, &repo, error),
        };
        if let Some(target) = target {
            return ctx.redirect(&format!(
                "{}/wiki/{}",
                repo.base(),
                crate::webui::encode_component(&target)
            ));
        }
        let empty_wiki = match wiki_db::list_pages(pool, repo.id, None).await {
            Ok(pages) => pages.is_empty(),
            Err(error) => return refused(ctx, &repo, error),
        };
        let title = if slug == "home" {
            "Home".to_owned()
        } else {
            slug.replace('-', " ")
        };
        return ctx.page_model_status(
            StatusCode::NOT_FOUND,
            &title.clone(),
            repo_page(
                repo.clone(),
                WikiPageView::Missing {
                    title,
                    can_write: repo.can_write,
                    empty_wiki,
                },
            ),
        );
    };
    let known = match wiki_db::list_pages(pool, repo.id, None).await {
        Ok(pages) => pages.into_iter().map(|page| page.slug).collect(),
        Err(error) => return refused(ctx, &repo, error),
    };
    let sidebar = wiki_db::get_page_by_slug(pool, repo.id, "_sidebar").await;
    let footer = wiki_db::get_page_by_slug(pool, repo.id, "_footer").await;
    let (sidebar, footer) = match (sidebar, footer) {
        (Ok(sidebar), Ok(footer)) => (
            sidebar.as_ref().map(page_item),
            footer.as_ref().map(page_item),
        ),
        (Err(error), _) | (_, Err(error)) => return refused(ctx, &repo, error),
    };
    let title = page.head.title.clone();
    ctx.page_model(
        &title,
        repo_page(
            repo.clone(),
            WikiPageView::Show {
                page: page_item(&page),
                sidebar,
                footer,
                known,
                can_write: repo.can_write,
                can_admin: repo.can_admin,
            },
        ),
    )
}

pub(crate) async fn home(
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    show_slug(&ctx, repo, "home").await
}

pub(crate) async fn show(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    show_slug(&ctx, repo, &slug).await
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let query = ctx
        .query
        .get("q")
        .map(|q| q.trim().to_owned())
        .unwrap_or_default();
    match wiki_db::list_pages(
        &ctx.state.pool,
        repo.id,
        (!query.is_empty()).then_some(query.as_str()),
    )
    .await
    {
        Ok(pages) => {
            let items = pages
                .into_iter()
                .map(|page| ListItem {
                    slug: page.slug,
                    title: page.title,
                    updated_at: page.updated_at.to_rfc3339(),
                })
                .collect();
            let can_write = repo.can_write;
            ctx.page_model(
                "All pages",
                repo_page(
                    repo,
                    WikiPageView::List {
                        items,
                        query,
                        can_write,
                    },
                ),
            )
        }
        Err(error) => refused(&ctx, &repo, error),
    }
}

pub(crate) async fn history(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    match wiki_db::list_revisions(&ctx.state.pool, page.id).await {
        Ok(revisions) => {
            let items = revisions.iter().map(revision_item).collect();
            ctx.page_model(
                "History",
                repo_page(
                    repo,
                    WikiPageView::History {
                        slug: page.slug,
                        title: page.head.title,
                        revisions: items,
                    },
                ),
            )
        }
        Err(error) => refused(&ctx, &repo, error),
    }
}

pub(crate) async fn revision(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug, number)): Path<(String, String, String, i64)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    let found = wiki_db::get_revision(&ctx.state.pool, page.id, number).await;
    let found = match found {
        Ok(found) => found,
        Err(error) => return refused(&ctx, &repo, error),
    };
    let Some(found) = found else {
        return ctx.repo_error(&repo, StatusCode::NOT_FOUND, "Revision not found.");
    };
    let known = match wiki_db::list_pages(&ctx.state.pool, repo.id, None).await {
        Ok(pages) => pages.into_iter().map(|page| page.slug).collect(),
        Err(error) => return refused(&ctx, &repo, error),
    };
    let title = found.title.clone();
    ctx.page_model(
        &title,
        repo_page(
            repo,
            WikiPageView::Revision {
                slug: page.slug.clone(),
                is_head: number == page.head.number,
                revision: revision_item(&found),
                known,
            },
        ),
    )
}

pub(crate) async fn compare(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug, old, new)): Path<(String, String, String, i64, i64)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let (old, new) = match (
        wiki_db::get_revision(pool, page.id, old).await,
        wiki_db::get_revision(pool, page.id, new).await,
    ) {
        (Ok(Some(old)), Ok(Some(new))) => (old, new),
        (Ok(_), Ok(_)) => {
            return ctx.repo_error(&repo, StatusCode::NOT_FOUND, "Revision not found.")
        }
        (Err(error), _) | (_, Err(error)) => return refused(&ctx, &repo, error),
    };
    let diff: Vec<DiffLine> = view::line_diff(&old.body, &new.body);
    ctx.page_model(
        "Compare revisions",
        repo_page(
            repo,
            WikiPageView::Compare {
                slug: page.slug,
                old: revision_item(&old),
                new: revision_item(&new),
                diff,
            },
        ),
    )
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    if let Err(response) = writer(&ctx, &repo) {
        return response;
    }
    let draft = DraftFields {
        title: ctx.query.get("title").cloned().unwrap_or_default(),
        ..DraftFields::default()
    };
    ctx.page_model(
        "New page",
        repo_page(
            repo,
            WikiPageView::Form {
                editing: None,
                draft,
                error: None,
            },
        ),
    )
}

/// Saves a new page. A refused title or address is shown in the form, with the text kept. A
/// preview shows the body rendered in the form and saves nothing.
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let actor = match writer(&ctx, &repo) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let draft = draft_of(&form);
    if draft.preview {
        return form_again(&ctx, &repo, None, draft, None, StatusCode::OK);
    }
    let checked = match rules::check(&draft.title, &draft.body, &draft.message) {
        Ok(checked) => checked,
        Err(message) => {
            return form_again(
                &ctx,
                &repo,
                None,
                draft,
                Some(message),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
        }
    };
    let content = WikiContent {
        slug: &checked.slug,
        title: &checked.title,
        body: &checked.body,
        message: &checked.message,
    };
    match wiki_db::create_page(&ctx.state.pool, actor, repo.id, &content).await {
        Ok(page) => ctx.redirect(&format!(
            "{}/wiki/{}",
            repo.base(),
            crate::webui::encode_component(&page.slug)
        )),
        Err(error) => match error.downcast_ref::<WikiRefusal>() {
            Some(WikiRefusal::SlugTaken | WikiRefusal::PageLimit) => {
                let message = error.to_string();
                form_again(
                    &ctx,
                    &repo,
                    None,
                    draft,
                    Some(message.as_str()),
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
            }
            _ => refused(&ctx, &repo, error),
        },
    }
}

/// The editor again, with what the writer typed and the reason it was refused, if any.
fn form_again(
    ctx: &PageContext,
    repo: &RepoChrome,
    editing: Option<(String, i64)>,
    draft: DraftFields,
    message: Option<&str>,
    status: StatusCode,
) -> Response {
    let title = if editing.is_some() {
        "Edit page"
    } else {
        "New page"
    };
    ctx.page_model_status(
        status,
        title,
        repo_page(
            repo.clone(),
            WikiPageView::Form {
                editing,
                draft,
                error: message.map(str::to_owned),
            },
        ),
    )
}

pub(crate) async fn edit_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    if let Err(response) = writer(&ctx, &repo) {
        return response;
    }
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    let draft = DraftFields {
        title: page.head.title.clone(),
        body: page.head.body.clone(),
        message: String::new(),
        preview: false,
    };
    ctx.page_model(
        "Edit page",
        repo_page(
            repo,
            WikiPageView::Form {
                editing: Some((page.slug.clone(), page.head.number)),
                draft,
                error: None,
            },
        ),
    )
}

/// Saves an edit against the head the writer opened. A page that changed since then is not
/// overwritten: the form comes back with the text kept and the current head to save against.
/// A preview comes back the same way, with the body rendered, and saves nothing.
pub(crate) async fn update(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let actor = match writer(&ctx, &repo) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    let draft = draft_of(&form);
    let expected: i64 = field(&form, "revision").parse().unwrap_or(-1);
    if draft.preview {
        let editing = Some((page.slug.clone(), expected));
        return form_again(&ctx, &repo, editing, draft, None, StatusCode::OK);
    }
    let checked = match rules::check(&draft.title, &draft.body, &draft.message) {
        Ok(checked) => checked,
        Err(message) => {
            let editing = Some((page.slug.clone(), expected));
            return form_again(
                &ctx,
                &repo,
                editing,
                draft,
                Some(message),
                StatusCode::UNPROCESSABLE_ENTITY,
            );
        }
    };
    let content = WikiContent {
        slug: &checked.slug,
        title: &checked.title,
        body: &checked.body,
        message: &checked.message,
    };
    match wiki_db::edit_page(&ctx.state.pool, actor, repo.id, page.id, expected, &content).await {
        Ok(saved) => ctx.redirect(&format!(
            "{}/wiki/{}",
            repo.base(),
            crate::webui::encode_component(&saved.slug)
        )),
        Err(error) => match error.downcast_ref::<WikiRefusal>() {
            Some(WikiRefusal::Stale) => {
                let current = match wiki_db::get_page_by_slug(&ctx.state.pool, repo.id, &page.slug)
                    .await
                {
                    Ok(Some(current)) => current.head.number,
                    Ok(None) => {
                        return ctx.repo_error(&repo, StatusCode::NOT_FOUND, "Wiki page not found.")
                    }
                    Err(error) => return refused(&ctx, &repo, error),
                };
                let editing = Some((page.slug.clone(), current));
                form_again(
                    &ctx,
                    &repo,
                    editing,
                    draft,
                    Some("The page changed while you were editing. Your text is kept below. Review the current page, then save again."),
                    StatusCode::CONFLICT,
                )
            }
            Some(WikiRefusal::SlugTaken) => {
                let editing = Some((page.slug.clone(), expected));
                form_again(
                    &ctx,
                    &repo,
                    editing,
                    draft,
                    Some("A page with this title already exists."),
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
            }
            _ => refused(&ctx, &repo, error),
        },
    }
}

pub(crate) async fn delete_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
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
    if let Err(response) = enabled(&ctx, &repo) {
        return response;
    }
    let actor = match administrator(&ctx, &repo) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match load_page(&ctx, &repo, &slug).await {
        Ok(page) => page,
        Err(response) => return response,
    };
    match wiki_db::delete_page(&ctx.state.pool, actor, repo.id, page.id).await {
        Ok(()) => ctx.redirect(&format!("{}/wiki", repo.base())),
        Err(error) => refused(&ctx, &repo, error),
    }
}

/// Turns the wiki on or off. The form posts `enabled` as `1` or `0`; the Settings page sends it.
pub(crate) async fn set_enabled(
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
    let actor = match administrator(&ctx, &repo) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let on = field(&form, "enabled") == "1";
    match wiki_db::set_wiki_enabled(&ctx.state.pool, actor, repo.id, on).await {
        Ok(()) => ctx.redirect(&format!("{}/settings", repo.base())),
        Err(error) => refused(&ctx, &repo, error),
    }
}
