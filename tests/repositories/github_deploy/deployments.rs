use super::*;
use codeyard_identity::tokens;

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let repo = codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        let issued = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", issued.value));
    }
    let (writer, reader, outsider) = (&credentials[0], &credentials[1], &credentials[2]);
    let base = "/api/v3/repos/alice/private";
    let deployments = format!("{base}/deployments");
    let (_, file) = checked(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        &format!("{base}/contents/hello.txt"),
        Some(writer),
        json!({"message":"Initial","content":"aGVsbG8K"}),
        201,
    )
    .await;
    let first = file["commit"]["sha"].as_str().unwrap();
    checked(
        &router,
        "git/create-ref",
        "POST",
        &format!("{base}/git/refs"),
        Some(writer),
        json!({"ref":"refs/heads/topic","sha":first}),
        201,
    )
    .await;
    checked(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        &format!("{base}/contents/other.txt"),
        Some(writer),
        json!({"message":"Advance main","content":"bmV3Cg=="}),
        201,
    )
    .await;
    checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic"}),
        202,
    )
    .await;
    let (_, empty) = checked(
        &router,
        "repos/list-deployments",
        "GET",
        &deployments,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty, json!([]));
    let (_, old) = checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic","environment":"staging","payload":{"version":1}}),
        201,
    )
    .await;
    assert_ne!(old["sha"], first);
    assert_eq!(old["payload"], json!({"version":1}));
    assert_eq!(old["creator"]["id"], alice);
    assert_eq!(old["production_environment"], false);
    let old_path = format!("{deployments}/{}", old["id"]);
    let old_statuses = format!("{old_path}/statuses");
    let (headers, success) = checked(&router,"repos/create-deployment-status","POST",&old_statuses,Some(writer),json!({"state":"success","target_url":"https://logs.example/old","environment_url":"https://staging.example/"}),201).await;
    assert_eq!(headers["location"], success["url"].as_str().unwrap());
    assert_eq!(success["log_url"], success["target_url"]);
    let (_, new) = checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic","environment":"staging"}),
        201,
    )
    .await;
    let new_path = format!("{deployments}/{}", new["id"]);
    let statuses = format!("{new_path}/statuses");
    let (_, status) = checked(&router,"repos/create-deployment-status","POST",&statuses,Some(writer),json!({"state":"success","log_url":"https://logs.example/new","target_url":"https://ignored.example/"}),201).await;
    assert_eq!(status["log_url"], "https://logs.example/new");
    assert_eq!(status["log_url"], status["target_url"]);
    let (headers, history) = checked(
        &router,
        "repos/list-deployment-statuses",
        "GET",
        &format!("{old_statuses}?per_page=1"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(history[0]["state"], "inactive");
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, got) = checked(
        &router,
        "repos/get-deployment-status",
        "GET",
        &format!("{statuses}/{}", status["id"]),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(got, status);
    checked(
        &router,
        "repos/get-deployment-status",
        "GET",
        &format!("{old_statuses}/{}", status["id"]),
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "repos/delete-deployment",
        "DELETE",
        &new_path,
        Some(writer),
        Value::Null,
        422,
    )
    .await;
    checked(
        &router,
        "repos/delete-deployment",
        "DELETE",
        &old_path,
        Some(writer),
        Value::Null,
        204,
    )
    .await;
    checked(
        &router,
        "repos/get-deployment",
        "GET",
        &old_path,
        Some(reader),
        Value::Null,
        404,
    )
    .await;
    assert!(codeyard_db_actions::deployments::statuses(
        f.pool(),
        None,
        old["id"].as_i64().unwrap(),
        0,
        10
    )
    .await
    .unwrap()
    .is_empty());
    let (_, failed) = checked(
        &router,
        "repos/create-deployment-status",
        "POST",
        &statuses,
        Some(writer),
        json!({"state":"failure","environment":"preview"}),
        201,
    )
    .await;
    assert_eq!(failed["environment"], "preview");
    let (_, got) = checked(
        &router,
        "repos/get-deployment",
        "GET",
        &new_path,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(got["environment"], "preview");
    assert_eq!(got["original_environment"], "staging");
    let (_, got) = checked(
        &router,
        "repos/create-deployment-status",
        "POST",
        &statuses,
        Some(writer),
        json!({"state":"pending"}),
        201,
    )
    .await;
    assert_eq!(got["environment"], "preview");
    for (suffix, input, operation) in [
        ("", json!({"ref":"topic"}), "repos/create-deployment"),
        (
            &format!("/{}", new["id"]),
            Value::Null,
            "repos/delete-deployment",
        ),
        (
            &format!("/{}/statuses", new["id"]),
            json!({"state":"failure"}),
            "repos/create-deployment-status",
        ),
    ] {
        let method = if operation == "repos/delete-deployment" {
            "DELETE"
        } else {
            "POST"
        };
        for (token, expected) in [
            (Some(reader.as_str()), 403),
            (Some(outsider.as_str()), 404),
            (None, 401),
        ] {
            checked(
                &router,
                operation,
                method,
                &format!("{deployments}{suffix}"),
                token,
                input.clone(),
                expected,
            )
            .await;
        }
    }
    for (operation, path) in [
        ("repos/list-deployments", deployments.clone()),
        ("repos/get-deployment", new_path.clone()),
        ("repos/list-deployment-statuses", statuses.clone()),
        (
            "repos/get-deployment-status",
            format!("{statuses}/{}", status["id"]),
        ),
    ] {
        checked(
            &router,
            operation,
            "GET",
            &path,
            Some(outsider),
            Value::Null,
            404,
        )
        .await;
    }
    for input in [
        json!({"state":"destroyed"}),
        json!({"state":"success","description":"x".repeat(141)}),
        json!({"state":"success","log_url":"not a URL"}),
        json!({"state":"success","environment":""}),
    ] {
        checked(
            &router,
            "repos/create-deployment-status",
            "POST",
            &statuses,
            Some(writer),
            input,
            422,
        )
        .await;
    }
    for input in [
        json!({"ref":"missing"}),
        json!({"ref":"topic","payload":42}),
        json!({"ref":"topic","environment":""}),
        json!({"ref":"topic","task":""}),
        json!({}),
    ] {
        checked(
            &router,
            "repos/create-deployment",
            "POST",
            &deployments,
            Some(writer),
            input,
            422,
        )
        .await;
    }
    let sha = new["sha"].as_str().unwrap();
    checked(
        &router,
        "repos/create-commit-status",
        "POST",
        &format!("{base}/statuses/{sha}"),
        Some(writer),
        json!({"state":"pending","context":"ci"}),
        201,
    )
    .await;
    checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic"}),
        409,
    )
    .await;
    checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic","required_contexts":["missing"]}),
        409,
    )
    .await;
    let (_, production) = checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic","required_contexts":[]}),
        201,
    )
    .await;
    assert_eq!(production["production_environment"], true);
    let production_statuses = format!("{deployments}/{}/statuses", production["id"]);
    checked(
        &router,
        "repos/create-deployment-status",
        "POST",
        &production_statuses,
        Some(writer),
        json!({"state":"success"}),
        201,
    )
    .await;
    checked(
        &router,
        "repos/create-commit-status",
        "POST",
        &format!("{base}/statuses/{sha}"),
        Some(writer),
        json!({"state":"success","context":"ci"}),
        201,
    )
    .await;
    let (_, final_deploy) = checked(
        &router,
        "repos/create-deployment",
        "POST",
        &deployments,
        Some(writer),
        json!({"ref":"topic","required_contexts":["ci"],"payload":"extra"}),
        201,
    )
    .await;
    let final_statuses = format!("{deployments}/{}/statuses", final_deploy["id"]);
    checked(
        &router,
        "repos/create-deployment-status",
        "POST",
        &final_statuses,
        Some(writer),
        json!({"state":"success"}),
        201,
    )
    .await;
    let (_, unchanged) = checked(
        &router,
        "repos/list-deployment-statuses",
        "GET",
        &production_statuses,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(unchanged[0]["state"], "success");
    let (headers, page) = checked(
        &router,
        "repos/list-deployments",
        "GET",
        &format!("{deployments}?per_page=1&ref=topic&environment=production"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page[0]["id"], final_deploy["id"]);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let (_, none) = checked(
        &router,
        "repos/list-deployments",
        "GET",
        &format!("{deployments}?task=missing"),
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(none, json!([]));
    // The effectful database entry point must not borrow the owner's token authority.
    let denied = codeyard_db_actions::deployments::create_status(
        f.pool(),
        bob,
        repo.id,
        new["id"].as_i64().unwrap(),
        &serde_json::from_value(json!({"state":"success"})).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<codeyard_db_actions::deployments::Refusal>(),
        Some(codeyard_db_actions::deployments::Refusal::Denied)
    ));
}

#[tokio::test]
async fn sqlite_github_deployments() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_deployments() {
    contract(true).await;
}
