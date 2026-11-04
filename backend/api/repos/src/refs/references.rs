//! The references a page's Markdown may link, resolved for its viewer. The page model carries the
//! answer, so the server and the browser render the same links, and a repository the viewer
//! cannot read never reaches the page.

use crate::discussion::unavailable;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_identity::repository_access::can_read;
use codeyard_webui::components::references::References;

/// The references `texts` make, as the viewer of `repository`, named `owner/name`, may link them.
/// `document` is the mode the page renders the texts with: a repository file is a document, and
/// anything else is a comment.
pub(crate) async fn resolve_references(
    ctx: &PageContext,
    repository: &codeyard_db_core::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
) -> Result<References, Response> {
    resolve_references_with(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        repository,
        owner,
        name,
        texts,
        document,
        |records| async move {
            let mut readable = std::collections::HashSet::new();
            for record in records {
                if can_read(&ctx.state.pool, ctx.user.as_ref(), &record).await? {
                    readable.insert(record.id);
                }
            }
            Ok(readable)
        },
    )
    .await
    .map_err(|_| unavailable(ctx))
}

/// The references of a page that shows the user's submitted text again, such as a preview or a
/// rejected save. That text must survive the page, so a failed lookup links nothing rather than
/// failing the page.
pub(crate) async fn resolve_references_or_none(
    ctx: &PageContext,
    repository: &codeyard_db_core::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
) -> References {
    resolve_references(ctx, repository, owner, name, texts, document)
        .await
        .unwrap_or_default()
}

use codeyard_api_http::shared::repos::refs::references::resolve_references_with;
