//! Deployment reviews retain the native revision and reviewer checks.

use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

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
    let write = !crate::backend::api::github::auth::read_method(request.method());
    // Required environment reviewers need only repository read access.
    let (repo, auth) = repository(&state, &headers, &owner, &name, write).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let run = crate::backend::api::github::actions::run(&state, repo.id, id).await?;
    if write {
        let actor = auth
            .ok_or_else(crate::backend::api::github::auth::required)?
            .database_actor();
        let Json(input) =
            Json::<codeyard_db_actions::deployment_reviews::Input>::from_request(request, &state)
                .await
                .map_err(|_| Error::validation("DeploymentReview", "configuration", "invalid"))?;
        let rows = codeyard_db_actions::deployment_reviews::review(
            &state.pool,
            actor,
            repo.id,
            run.id,
            &input,
        )
        .await
        .map_err(crate::backend::api::github::actions::settings::failure)?;
        let mut values = Vec::new();
        for row in rows {
            values.push(
                crate::backend::api::github::deployments::document(&state, &row, &owner, &name)
                    .await?,
            );
        }
        return Ok(Json(json!(values)));
    }
    let rows =
        codeyard_db_actions::deployment_reviews::pending(&state.pool, repo.id, run.id, viewer)
            .await
            .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        let environment = crate::backend::api::github::environments::document(
            &state,
            viewer,
            &row.environment,
            &owner,
            &name,
        )
        .await?;
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
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let run = crate::backend::api::github::actions::run(&state, repo.id, id).await?;
    let rows = codeyard_db_actions::deployment_reviews::history(&state.pool, repo.id, run.id)
        .await
        .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in rows {
        let user = if let Some(actor) = row.actor_id {
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, actor)
                .await
                .map_err(Error::unavailable)?
                .map(|user| crate::backend::webhooks::payloads::user(&user))
        } else {
            None
        }
        .unwrap_or_else(|| {
            crate::backend::webhooks::payloads::person(
                &crate::backend::webhooks::payloads::Person {
                    login: "ghost".into(),
                    id: 0,
                    site_admin: false,
                },
            )
        });
        let encoded = codeyard_webui::encode_component(&row.environment_name);
        let environment = json!({"id":row.environment_id,
            "node_id":crate::backend::api::github::node_id::encode("EN", row.environment_id),
            "name":row.environment_name,
            "url":format!("{}/environments/{encoded}", codeyard_model::urls::repo_url(&owner, &name)),
            "html_url":format!("{}/{}/{}/deployments/activity_log?environments_filter={encoded}", codeyard_model::http_config::link_origin(), codeyard_webui::encode_component(&owner), codeyard_webui::encode_component(&name))});
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
