use super::*;

fn workflow(types: Option<&str>) -> String {
    let trigger = types.map_or("  repository_dispatch:\n".to_owned(), |kind| {
        format!("  repository_dispatch:\n    types: [{kind}]\n")
    });
    format!("name: Event check\non:\n{trigger}jobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo event\n")
}
async fn dispatch_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    // Host execution approval is a real native policy, separate from write permission.
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[alice])
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, bob, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let mut tip = git2::Repository::open_bare(&repo.disk_path)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    for (name, types) in [
        ("typed.yml", Some("rebuild")),
        ("always.yml", None),
        ("other.yml", Some("other")),
    ] {
        tip = owner
            .save_text(
                "main",
                &format!(".github/workflows/{name}"),
                &workflow(types),
                &tip,
            )
            .await;
    }
    owner.branch("feature", &tip).await;
    owner
        .save_text(
            "feature",
            ".github/workflows/feature-only.yml",
            &workflow(None),
            &tip,
        )
        .await;
    assert!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let endpoint = "/api/v3/repos/alice/proj/dispatches";
    let client_payload =
        json!({"enabled":true,"matrix":[1,{"target":"prod"}],"text":"</script> is data"});
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        endpoint,
        Some(&writer),
        json!({"event_type":"rebuild","client_payload":client_payload,"branch":"feature"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    for run in &runs {
        assert_eq!(run.event, "repository_dispatch");
        assert_eq!(run.branch, "main");
        assert_eq!(run.head_sha.as_deref(), Some(tip.as_str()));
        assert!(run.workflow_inputs.is_none());
        assert!(
            run.workflow_path.ends_with("typed.yml") || run.workflow_path.ends_with("always.yml")
        );
        let event = run.event_payload.as_ref().unwrap();
        assert_eq!(event["action"], "rebuild");
        assert_eq!(event["client_payload"], client_payload);
        assert_eq!(event["branch"], "main");
        assert_eq!(event["repository"]["full_name"], "alice/proj");
        assert_eq!(event["sender"]["login"], "alice");
    }
    let claimed = codeyard_db_actions::ci::claim_ci_run(
        f.pool(),
        "dispatch-worker",
        "local",
        &[],
        &codeyard_app::ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.repo_id, repo.id);
    assert_eq!(claimed.event, "repository_dispatch");
    assert_eq!(claimed.status, "running");
    assert_eq!(
        claimed.event_payload.as_ref().unwrap()["client_payload"],
        client_payload
    );
    for (token, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(reader.as_str()), StatusCode::FORBIDDEN),
        (Some(outsider.as_str()), StatusCode::NOT_FOUND),
    ] {
        checked(
            &router,
            "repos/create-dispatch-event",
            "POST",
            endpoint,
            token,
            json!({"event_type":"rebuild"}),
            status,
        )
        .await;
    }
    for body in [
        json!({}),
        json!({"event_type":42}),
        json!({"event_type":""}),
        json!({"event_type":"x".repeat(101)}),
        json!({"event_type":"line\nbreak"}),
        json!({"event_type":"rebuild","client_payload":null}),
        json!({"event_type":"rebuild","client_payload":[]}),
        json!({"event_type":"rebuild","client_payload":{"x":"a".repeat(65536)}}),
        json!({"event_type":"rebuild","client_payload":(0..11).map(|i|(format!("key{i}"),json!(i))).collect::<serde_json::Map<_,_>>()}),
    ] {
        checked(
            &router,
            "repos/create-dispatch-event",
            "POST",
            endpoint,
            Some(&writer),
            body,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    // A repository writer without host execution approval can send an event but cannot
    // use the endpoint to run code on the site's machines.
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        endpoint,
        Some(&outsider),
        json!({"event_type":"rebuild"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    let form = "/alice/proj/actions/dispatch-event";
    assert!(owner
        .page_ok("/alice/proj/actions")
        .await
        .contains("Send repository event"));
    let html = owner.page_ok(form).await;
    assert!(html.contains("client_payload"));
    assert!(html.contains("repository_id"));
    let rejected = owner
        .form_reply(
            form,
            &[
                ("repository_id", &repo.id.to_string()),
                ("event_type", "retry-event"),
                ("client_payload", "{broken"),
            ],
        )
        .await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let html = rejected.text();
    assert!(html.contains("retry-event"));
    assert!(html.contains("{broken"));
    let rejected = owner
        .form_reply(
            form,
            &[
                ("repository_id", "0"),
                ("event_type", "rebuild"),
                ("client_payload", "{}"),
            ],
        )
        .await;
    assert_eq!(rejected.status, StatusCode::CONFLICT);
    let rejected = owner
        .send(
            "POST",
            form,
            "application/x-www-form-urlencoded",
            format!("repository_id={}&event_type=rebuild", repo.id),
        )
        .await;
    assert_eq!(rejected.0, StatusCode::FORBIDDEN);
    let accepted = owner
        .form_reply(
            form,
            &[
                ("repository_id", &repo.id.to_string()),
                ("event_type", "rebuild"),
                ("client_payload", r#"{"native":true}"#),
            ],
        )
        .await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 4);
    assert_eq!(
        runs[0].event_payload.as_ref().unwrap()["client_payload"]["native"],
        true
    );
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        endpoint,
        Some(&writer),
        json!({"event_type":"rebuild"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let runs = codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 6);
    assert_eq!(
        runs[0].event_payload.as_ref().unwrap()["client_payload"],
        json!({})
    );
    // Actions disabled means no execution, not a fabricated run or a failure to accept an event.
    codeyard_db_actions::actions::save_repo_actions_settings(
        f.pool(),
        alice,
        repo.id,
        &codeyard_db_actions::actions::RepoActionsSettings {
            enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        endpoint,
        Some(&writer),
        json!({"event_type":"rebuild"}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        codeyard_db_actions::ci::list_ci_runs_for_repo(f.pool(), repo.id, 100)
            .await
            .unwrap()
            .len(),
        6
    );
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/proj",
        Some(&writer),
        json!({"archived":true}),
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        endpoint,
        Some(&writer),
        json!({"event_type":"rebuild"}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let (_, quiet) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"quiet"}),
        StatusCode::CREATED,
    )
    .await;
    let quiet_endpoint = "/api/v3/repos/alice/quiet/dispatches";
    checked(
        &router,
        "repos/create-dispatch-event",
        "POST",
        quiet_endpoint,
        Some(&writer),
        json!({"event_type":"雪".repeat(100),"client_payload":{"x":"a".repeat(65528)}}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(codeyard_db_actions::ci::list_ci_runs_for_repo(
        f.pool(),
        quiet["id"].as_i64().unwrap(),
        100
    )
    .await
    .unwrap()
    .is_empty());
}
#[tokio::test]
async fn sqlite_github_repository_dispatch() {
    dispatch_contract(false).await;
}
#[tokio::test]
async fn postgres_github_repository_dispatch() {
    dispatch_contract(true).await;
}
