use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_app::app::AppState;
use codeyard_app::ci::runs::DispatchRequest;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct TriggerCiResponse {
    pub job_id: String,
    pub mode: String,
    pub pipeline: String,
    pub workflow_path: String,
    pub status: String,
    pub message: String,
}

pub async fn trigger_ci(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<DispatchRequest>,
) -> Result<Json<TriggerCiResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let run = state
        .ci_runs
        .dispatch_run(
            user.id,
            request,
            codeyard_api_http::session::extract_session_id(&headers)
                .map(codeyard_db_core::queries::ci::CiCredential::Session),
        )
        .await
        .map_err(codeyard_api_http::shared::ci::admission_error)?;
    Ok(Json(TriggerCiResponse {
        job_id: run.run_id.to_string(),
        mode: run.mode,
        pipeline: run.pipeline,
        workflow_path: run.workflow_path,
        status: run.status,
        message: "CI/CD job started".to_owned(),
    }))
}

pub(super) async fn global_dispatch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    dispatch(state, headers, uri, form, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_api_http::context::PageContext;
    use codeyard_api_http::shared::ci::trigger::draft;
    use codeyard_api_http::shared::ci::trigger::refusal_workflows;
    use codeyard_webui::pages::repos::actions as view_actions;
    #[test]
    fn dispatch_form_offers_only_events_the_server_accepts() {
        for event in view_actions::TRIGGER_EVENTS {
            assert!(
                codeyard_app::ci::admission::is_supported_event(event),
                "{event}"
            );
        }
    }
    #[tokio::test]
    async fn refusal_discovery_keeps_the_authorized_repository_after_alias_reuse() {
        use codeyard_app::storage::ObjectStore;
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                "sqlite::memory:"
                    .parse::<sqlx::sqlite::SqliteConnectOptions>()
                    .unwrap()
                    .with_regexp(),
            )
            .await
            .unwrap();
        let pool = codeyard_db_core::DbPool::from(native.clone());
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        codeyard_db_cluster::node_config::upsert_node_configuration(&pool, "public", true)
            .await
            .unwrap();
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let original = codeyard_app::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "project",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let private = codeyard_app::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "private",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        for (repository, name) in [(&original, "Allowed"), (&private, "Secret")] {
            let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
            let blob = git.blob(format!("name: {name}\non: workflow_dispatch\njobs: {{test: {{steps: [{{run: echo ok}}]}}}}\n").as_bytes()).unwrap();
            let mut workflows = git.treebuilder(None).unwrap();
            workflows.insert("ci.yml", blob, 0o100644).unwrap();
            let mut github = git.treebuilder(None).unwrap();
            github
                .insert("workflows", workflows.write().unwrap(), 0o040000)
                .unwrap();
            let mut tree = git.treebuilder(None).unwrap();
            tree.insert(".github", github.write().unwrap(), 0o040000)
                .unwrap();
            let tree = git.find_tree(tree.write().unwrap()).unwrap();
            let sig = git2::Signature::now("Alice", "alice@example.test").unwrap();
            git.commit(Some("refs/heads/main"), &sig, &sig, "workflow", &tree, &[])
                .unwrap();
        }
        let state = AppState::new(
            pool.clone(),
            root.path().to_owned(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            codeyard_app::ci::config::CiConfig::default(),
            None,
        );
        // Only a signed-in reader is shown a refused run's workflows; bob may read the public one.
        // The page reads him from a live session, as metadata is checked against the cookie.
        let bob = codeyard_db_identity::users::accounts::create_user(&pool, "bob", None)
            .await
            .unwrap();
        let session = uuid::Uuid::new_v4().simple().to_string().repeat(2);
        codeyard_db_identity::users::sessions::create_session(
            &pool,
            &session,
            bob.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("session_id={session}").parse().unwrap(),
        );
        let ctx = PageContext::load(
            Arc::new(state),
            headers,
            &"/alice/project/actions".parse().unwrap(),
        )
        .await
        .unwrap_or_else(|_| panic!("page context"));
        assert_eq!(ctx.user.as_ref().map(|user| user.id), Some(bob.id));
        let loaded =
            codeyard_api_http::shared::repos::chrome::load_metadata(&ctx, "alice", "project")
                .await
                .unwrap_or_else(|_| panic!("public metadata"));
        sqlx::query("UPDATE repositories SET name='renamed' WHERE id=?1")
            .bind(original.id)
            .execute(&native)
            .await
            .unwrap();
        sqlx::query("UPDATE repositories SET name='project' WHERE id=?1")
            .bind(private.id)
            .execute(&native)
            .await
            .unwrap();
        let workflows = refusal_workflows(&ctx, &loaded.repository, "main")
            .await
            .unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].name, "Allowed");
        pool.close().await;
    }

    #[test]
    fn repository_dispatch_uses_route_identity_not_hidden_fields() {
        let form = HashMap::from([
            ("repo_owner".into(), "other".into()),
            ("repo_name".into(), "private".into()),
            ("event".into(), "release".into()),
            ("branch".into(), "feature/x".into()),
        ]);
        let scope = ("alice".into(), "repo".into());
        let draft = draft(&form, Some(&scope));
        assert_eq!(draft.owner, "alice");
        assert_eq!(draft.repository, "repo");
        assert_eq!(draft.event, "workflow_dispatch");
        assert_eq!(draft.branch, "feature/x");
    }
}

use codeyard_api_http::shared::ci::trigger::dispatch;
