use super::{db, json, protection::call, request, Router, Value};

pub(super) async fn run(
    router: &Router,
    scope: (&db::DbPool, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
) {
    let (writer, reader, outsider) = tokens;
    let policy = format!("{base}/branches/main/protection");
    let signatures = format!("{policy}/required_signatures");
    call(router, "PUT", &policy, writer, json!({"required_status_checks":null,"enforce_admins":true,"required_pull_request_reviews":null,"restrictions":null}), ("repos/update-branch-protection", 200)).await;
    call(
        router,
        "POST",
        &signatures,
        writer,
        Value::Null,
        ("repos/create-commit-signature-protection", 200),
    )
    .await;
    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    for signature in [Value::Null, json!("not a signature")] {
        let child = call(router, "POST", &format!("{base}/git/commits"), writer, json!({"message":"Unverified","tree":original["tree"]["sha"],"parents":[sha],"signature":signature}), ("git/create-commit", 201)).await;
        call(
            router,
            "PATCH",
            &format!("{base}/git/refs/heads/main"),
            writer,
            json!({"sha":child["sha"]}),
            ("git/update-ref", 403),
        )
        .await;
    }
    call(
        router,
        "PUT",
        &format!("{base}/contents/blocked"),
        writer,
        json!({"message":"Unsigned editor commit","content":"YQ=="}),
        ("repos/create-or-update-file-contents", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{base}/git/ref/heads/main"),
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        sha
    );
    call(
        router,
        "DELETE",
        &signatures,
        writer,
        Value::Null,
        ("repos/delete-commit-signature-protection", 204),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/feature","sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    call(
        router,
        "PUT",
        &format!("{base}/contents/feature"),
        writer,
        json!({"message":"Feature","branch":"feature","content":"ZmVhdHVyZQ=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    // A missing branch in a populated repository is not a new, unrelated history.
    call(
        router,
        "PUT",
        &format!("{base}/contents/stray"),
        writer,
        json!({"message":"Stray","branch":"featrue","content":"eA=="}),
        ("repos/create-or-update-file-contents", 404),
    )
    .await;
    let pull = call(
        router,
        "POST",
        &format!("{base}/pulls"),
        writer,
        json!({"title":"Rename tracking","head":"feature","base":"main"}),
        ("pulls/create", 201),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    call(router, "PUT", &format!("{base}/branches/feature/protection"), writer, json!({"required_status_checks":null,"enforce_admins":true,"required_pull_request_reviews":null,"restrictions":null}), ("repos/update-branch-protection", 200)).await;
    let renamed = call(
        router,
        "POST",
        &format!("{base}/branches/feature/rename"),
        writer,
        json!({"new_name":"topic"}),
        ("repos/rename-branch", 201),
    )
    .await;
    assert_eq!(renamed["protected"], true);
    assert_eq!(renamed["commit"]["sha"], pull["head"]["sha"]);
    call(
        router,
        "GET",
        &format!("{base}/branches/topic/protection"),
        reader,
        Value::Null,
        ("repos/get-branch-protection", 200),
    )
    .await;
    let bob = db::users::accounts::get_user_by_username(scope.0, "bob")
        .await
        .unwrap()
        .unwrap();
    db::repositories::access::grant_repo_access(
        scope.0,
        bob.id,
        scope.1,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    call(
        router,
        "POST",
        &format!("{base}/branches/main/rename"),
        outsider,
        json!({"new_name":"trunk"}),
        ("repos/rename-branch", 403),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{base}/branches/main/rename"),
        writer,
        json!({"new_name":"trunk"}),
        ("repos/rename-branch", 201),
    )
    .await;
    let pull = call(
        router,
        "GET",
        &format!("{base}/pulls/{number}"),
        reader,
        Value::Null,
        ("pulls/get", 200),
    )
    .await;
    assert_eq!(pull["head"]["ref"], "topic");
    assert_eq!(pull["base"]["ref"], "trunk");
    assert_eq!(
        call(router, "GET", base, reader, Value::Null, ("repos/get", 200)).await["default_branch"],
        "trunk"
    );
    let repo = db::repositories::records::get_repository_by_id(scope.0, scope.1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codeyard::backend::repositories::store::repo_store(&repo, None)
            .info()
            .await
            .unwrap()
            .default_branch,
        "trunk"
    );
    call(
        router,
        "GET",
        &format!("{base}/branches/trunk/protection"),
        reader,
        Value::Null,
        ("repos/get-branch-protection", 200),
    )
    .await;
    for target in ["topic", "renamed"] {
        if target == "renamed" {
            call(
                router,
                "POST",
                &format!("{base}/branches/topic/rename"),
                writer,
                json!({"new_name":target}),
                ("repos/rename-branch", 201),
            )
            .await;
        }
        let (status, headers, _) = request(
            router,
            "GET",
            &format!("{base}/branches/feature"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status.as_u16(), 301);
        assert!(headers["location"]
            .to_str()
            .unwrap()
            .ends_with(&format!("/branches/{target}")));
    }
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/feature","sha":sha}),
        ("git/create-ref", 201),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{base}/branches/feature"),
            reader,
            Value::Null,
            ("repos/get-branch", 200)
        )
        .await["commit"]["sha"],
        sha
    );
}
