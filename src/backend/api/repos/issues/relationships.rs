use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{repository, unavailable, ResultPage},
            forms::{authorize_post, field},
        },
    },
    app::AppState,
    db::{
        self,
        relationships::{Kind, Mutation, Refusal},
    },
};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_webui::pages::repos::issues::RelatedIssue;
use std::{collections::HashMap, sync::Arc};
pub(crate) async fn load(ctx: &PageContext, root: i64) -> ResultPage<Vec<RelatedIssue>> {
    let mut result = Vec::new();
    for kind in [
        Kind::Parent,
        Kind::Children,
        Kind::BlockedBy,
        Kind::Blocking,
        Kind::Related,
    ] {
        for link in db::relationships::list(
            &ctx.state.pool,
            ctx.user.as_ref().map(|u| u.id),
            root,
            kind,
            0,
            100,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        {
            let href = format!(
                "/{}/{}/issues/{}",
                codeyard_webui::encode_component(&link.owner),
                codeyard_webui::encode_component(&link.repo),
                link.number
            );
            result.push(RelatedIssue {
                id: link.id,
                kind: kind.name().into(),
                title: link.title,
                state: link.state,
                reference: format!("{}/{}#{}", link.owner, link.repo, link.number),
                href,
            });
        }
    }
    Ok(result)
}
pub(crate) async fn native_change(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let root = db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
        .await
        .map_err(|_| unavailable(&ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
    let kind = Kind::parse(&field(&form, "kind"))
        .filter(|kind| matches!(kind, Kind::Children | Kind::BlockedBy | Kind::Related))
        .ok_or_else(|| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid relationship."))?;
    let action = field(&form, "action");
    let other = if action == "remove" || action == "move" {
        field(&form, "issue_id")
            .parse()
            .map_err(|_| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid issue."))?
    } else {
        super::super::discussion::issue_reference(&ctx, (&owner, &name), &field(&form, "reference"))
            .await?
    };
    let mutation = match action.as_str() {
        "add" => Mutation::Add {
            kind,
            other,
            replace: form.get("replace_parent").is_some_and(|v| v == "true"),
        },
        "remove" => Mutation::Remove { kind, other },
        "move" if kind == Kind::Children => Mutation::Move {
            child: other,
            before: form.get("before_id").and_then(|v| v.parse().ok()),
            after: form.get("after_id").and_then(|v| v.parse().ok()),
        },
        _ => {
            return Err(ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid relationship action.",
            ))
        }
    };
    db::relationships::change(
        &ctx.state.pool,
        ctx.require_user()?.id,
        repo.record.id,
        root.id,
        mutation,
    )
    .await
    .map_err(|error| match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => {
            ctx.error(StatusCode::NOT_FOUND, "Issue or relationship not found.")
        }
        Some(Refusal::Denied) => ctx.error(StatusCode::FORBIDDEN, "Relationship mutation denied."),
        Some(reason) => ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &reason.to_string()),
        None => unavailable(&ctx),
    })?;
    Ok(ctx.redirect(&format!(
        "/{}/{}/issues/{number}",
        codeyard_webui::encode_component(&owner),
        codeyard_webui::encode_component(&name)
    )))
}
