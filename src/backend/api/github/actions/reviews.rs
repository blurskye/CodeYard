//! Deployment reviews retain the native revision and reviewer checks.
use crate::backend::{
    api::github::{auth, deployments, environments, repository, Error, Result},
    app::AppState,
    db::{self, deployment_reviews},
    webhooks::payloads,
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::HeaderMap,
    routing::{get, post, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

async fn pending(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    request: Request,
) -> Result<Json<Value>> {
    let write = !auth::read_method(request.method());
    // Required environment reviewers need only repository read access.
    let (repo, auth) = repository(&state, &headers, &owner, &name, write).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let run = super::run(&state, repo.id, id).await?;
    if write {
        let actor = auth.ok_or_else(auth::required)?.user.id;
        let Json(input) = Json::<deployment_reviews::Input>::from_request(request, &state)
            .await
            .map_err(|_| Error::validation("DeploymentReview", "configuration", "invalid"))?;
        let rows = deployment_reviews::review(&state.pool, actor, repo.id, run.id, &input)
            .await
            .map_err(super::settings::failure)?;
        let mut values = Vec::new();
        for row in rows {
            values.push(deployments::document(&state, &row, &owner, &name).await?);
        }
        return Ok(Json(json!(values)));
    }
    let rows = deployment_reviews::pending(&state.pool, repo.id, run.id, viewer)
        .await
        .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        let environment =
            environments::document(&state, viewer, &row.environment, &owner, &name).await?;
        let reviewers = environment["protection_rules"]
            .as_array()
            .and_then(|rules| {
                rules
                    .iter()
                    .find(|rule| rule["type"] == "required_reviewers")
            })
            .map(|rule| rule["reviewers"].clone())
            .unwrap_or_else(|| json!([]));
        values.push(json!({"environment":environment,"wait_timer":row.environment.wait_timer,"wait_timer_started_at":row.waiting_since,"current_user_can_approve":row.can_approve,"reviewers":reviewers}));
    }
    Ok(Json(json!(values)))
}

async fn history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let viewer = auth.map(|auth| auth.user.id);
    let run = super::run(&state, repo.id, id).await?;
    let rows = deployment_reviews::history(&state.pool, repo.id, run.id)
        .await
        .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        let user = if let Some(actor) = row.actor_id {
            db::users::accounts::get_user_by_id(&state.pool, actor)
                .await
                .map_err(Error::unavailable)?
                .map(|user| payloads::user(&user))
        } else {
            None
        }
        .unwrap_or_else(|| {
            payloads::person(&payloads::Person {
                login: "ghost".into(),
                id: 0,
                site_admin: false,
            })
        });
        let environment =
            environments::document(&state, viewer, &row.environment, &owner, &name).await?;
        values.push(json!({"environments":[environment],"state":row.state,"user":user,"comment":row.comment}));
    }
    Ok(Json(json!(values)))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-pending-deployments-for-run", get(pending)),
        ("actions/review-pending-deployments-for-run", post(pending)),
        ("actions/get-reviews-for-run", get(history)),
    ]
}
