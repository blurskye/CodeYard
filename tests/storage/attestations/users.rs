use super::*;
use axum::http::StatusCode;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("user-attestor").await;
    let bob = f.actor("user-observer").await;
    for (user, name, private) in [
        (alice, "public", false),
        (alice, "private", true),
        (bob, "elsewhere", false),
    ] {
        f.execute(&format!("INSERT INTO repositories(owner_kind,owner_user_id,name,disk_path,is_private) VALUES('user',$1,'{name}','/user-attestations/{name}.git',{private})"),&[user]).await;
    }
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = token(&f, &router, alice, "write").await;
    let reader = token(&f, &router, alice, "read").await;
    let visitor = token(&f, &router, bob, "write").await;
    let digest = format!("sha256:{}", "4".repeat(64));
    let second = format!("sha256:{}", "5".repeat(64));
    let input = json!({"bundle":bundle(&[digest.clone(),second.clone()],"https://slsa.dev/provenance/v1","shared signature")});
    let (_, public) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-attestor/public/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    let (_, private) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-attestor/private/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    let (_, other) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-observer/elsewhere/attestations",
        Some(&visitor),
        input.clone(),
        201,
    )
    .await;
    let base = "/api/v3/users/user-attestor/attestations";
    let oauth = oauth_token(&f, &router, alice, "repo").await;
    let wrong_scope = oauth_token(&f, &router, alice, "read:org").await;
    let (_, oauth_read) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}"),
        Some(&oauth),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(oauth_read["attestations"].as_array().unwrap().len(), 2);
    call(
        &router,
        "users/list-attestations-bulk",
        "POST",
        &format!("{base}/bulk-list"),
        Some(&oauth),
        json!({"subject_digests":[digest]}),
        200,
    )
    .await;
    call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}"),
        Some(&wrong_scope),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", public["id"]),
        Some(&wrong_scope),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}"),
        None,
        Value::Null,
        401,
    )
    .await;
    let (_, visible) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}?per_page=1"),
        Some(&visitor),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(visible["attestations"].as_array().unwrap().len(), 1);
    assert_eq!(visible["attestations"][0]["id"], public["id"]);
    let (headers, first) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}?per_page=1&predicate_type=provenance"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(first["attestations"][0]["id"], public["id"]);
    assert!(headers["link"]
        .to_str()
        .unwrap()
        .contains("predicate_type=provenance"));
    let after = URL_SAFE_NO_PAD.encode(format!("cursor:v1:{}", public["id"]));
    let (_, next) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}?per_page=1&after={after}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(next["attestations"][0]["id"], private["id"]);
    let before = URL_SAFE_NO_PAD.encode(format!("cursor:v1:{}", private["id"]));
    let (_, previous) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}?per_page=1&before={before}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(previous["attestations"][0]["id"], public["id"]);
    call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/{digest}?after=invalid"),
        Some(&reader),
        Value::Null,
        400,
    )
    .await;
    call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("{base}/bad-digest"),
        Some(&reader),
        Value::Null,
        422,
    )
    .await;
    let (_, bulk) = call(
        &router,
        "users/list-attestations-bulk",
        "POST",
        &format!("{base}/bulk-list?per_page=1"),
        Some(&reader),
        json!({"subject_digests":[digest,second],"predicate_type":"provenance"}),
        200,
    )
    .await;
    assert!(bulk["page_info"]["has_next"].as_bool().unwrap());
    assert_eq!(
        bulk["attestations_subject_digests"][&digest][0]["id"],
        public["id"]
    );
    let (_, filtered) = call(
        &router,
        "users/list-attestations-bulk",
        "POST",
        &format!("{base}/bulk-list"),
        Some(&visitor),
        json!({"subject_digests":[digest,second],"predicate_type":"sbom"}),
        200,
    )
    .await;
    assert!(filtered["attestations_subject_digests"][&digest]
        .as_array()
        .unwrap()
        .is_empty());
    for input in [
        json!({"subject_digests":[]}),
        json!({"subject_digests":["bad"]}),
        json!({"subject_digests":vec![digest.clone();1025]}),
    ] {
        call(
            &router,
            "users/list-attestations-bulk",
            "POST",
            &format!("{base}/bulk-list"),
            Some(&reader),
            input,
            422,
        )
        .await;
    }
    let owner = Client::login(&router, f.pool(), alice).await;
    let native = "/users/user-attestor/attestations";
    let html = Client::anonymous(&router).page_ok(native).await;
    assert!(html.contains("user-attestor/public"));
    assert!(!html.contains("user-attestor/private"));
    let (_, model) = owner.model(native).await;
    let codeyard_webui::app::Page::Attestations(page) = model else {
        panic!("user attestation model")
    };
    assert_eq!(page.entries.len(), 2);
    assert!(page
        .entries
        .iter()
        .all(|entry| entry.delete_action.is_some()));
    let missing_csrf = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(native)
                .header("cookie", format!("session_id={}", owner.session))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("attestation_id={}", public["id"])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", public["id"]),
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", public["id"]),
        Some(&visitor),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", other["id"]),
        Some(&writer),
        Value::Null,
        404,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", private["id"]),
        Some(&visitor),
        Value::Null,
        404,
    )
    .await;
    for input in [
        json!({"attestation_ids":[],"subject_digests":[digest]}),
        json!({"attestation_ids":[0]}),
        json!({"subject_digests":[]}),
    ] {
        call(
            &router,
            "users/delete-attestations-bulk",
            "POST",
            &format!("{base}/delete-request"),
            Some(&writer),
            input,
            422,
        )
        .await;
    }
    call(
        &router,
        "users/delete-attestations-by-id",
        "DELETE",
        &format!("{base}/{}", private["id"]),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-subject-digest",
        "DELETE",
        &format!("{base}/digest/{digest}"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-by-subject-digest",
        "DELETE",
        &format!("{base}/digest/{digest}"),
        Some(&writer),
        Value::Null,
        404,
    )
    .await;
    let (_, recreated) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-attestor/public/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-bulk",
        "POST",
        &format!("{base}/delete-request"),
        Some(&writer),
        json!({"attestation_ids":[recreated["id"]]}),
        200,
    )
    .await;
    call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-attestor/public/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    call(
        &router,
        "users/delete-attestations-bulk",
        "POST",
        &format!("{base}/delete-request"),
        Some(&writer),
        json!({"subject_digests":[digest]}),
        200,
    )
    .await;
    let (_, recreated) = call(
        &router,
        "repos/create-attestation",
        "POST",
        "/api/v3/repos/user-attestor/public/attestations",
        Some(&writer),
        input,
        201,
    )
    .await;
    assert_eq!(
        owner
            .form_reply(native, &[("attestation_id", &recreated["id"].to_string())])
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let (_, untouched) = call(
        &router,
        "users/list-attestations",
        "GET",
        &format!("/api/v3/users/user-observer/attestations/{digest}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(untouched["attestations"].as_array().unwrap().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_user_attestations() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_user_attestations() {
    contract(true).await;
}
