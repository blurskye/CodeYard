use super::*;
use axum::{http::StatusCode, Router};
pub(super) async fn contract(
    f: &Database,
    router: &Router,
    browser: &Client,
    owner: i64,
    org: i64,
    first: &db::models::Repository,
    write: Option<&str>,
) {
    let settings_page = format!("/settings/organizations/{org}?tab=settings");
    let action = format!("/settings/organizations/{org}/settings");
    let (status, page) = browser.page(&settings_page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("private-billing@example.test"));
    assert!(page.contains("Concurrent company"));
    let member = db::users::accounts::get_user_by_username(f.pool(), "member")
        .await
        .unwrap()
        .unwrap();
    let member_browser = Client::login(router, f.pool(), member.id).await;
    let (_, member_page) = member_browser.page(&settings_page).await;
    assert!(!member_page.contains("private-billing@example.test"));
    assert!(!member_page.contains("save-organization-contacts"));
    let (status, creation) = member_browser
        .model(&format!("/new?owner_org_id={org}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Discovery(
        codeyard::webui::pages::discovery::Page::NewRepository { form, choices, .. },
    ) = creation
    else {
        panic!("expected native creation page")
    };
    assert!(
        form.is_private,
        "a private-only organization starts with the permitted visibility"
    );
    assert!(choices.organizations.iter().any(|choice| choice.id == org));
    let source = org.to_string();
    let created = member_browser
        .form_reply(
            "/new",
            &[
                ("owner_org_id", &source),
                ("properties_owner_org_id", &source),
                ("name", "member-native"),
                ("visibility", "private"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let repository = db::repositories::records::get_repository(f.pool(), "acme", "member-native")
        .await
        .unwrap()
        .unwrap();
    assert!(repository.is_private);
    assert_eq!(
        db::repositories::access::get_user_repo_access(f.pool(), member.id, repository.id)
            .await
            .unwrap(),
        AccessLevel::Admin
    );
    let (status, profile) = member_browser.model("/acme").await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::OrganizationProfile(profile) = profile else {
        panic!("expected native organization profile")
    };
    assert!(profile.can_create);
    assert!(!profile.manage);
    let forbidden = member_browser
        .form_reply(
            &action,
            &[
                ("action", "save-organization-contacts"),
                ("company", "No authority"),
            ],
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let invalid = browser
        .form_reply(
            &action,
            &[
                ("action", "save-organization-contacts"),
                ("company", "Draft & preserved"),
                ("email", "invalid-address"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert!(invalid.text().contains("Draft &#38; preserved"));
    assert!(invalid.text().contains("invalid-address"));
    assert_eq!(
        data::get(f.pool(), org)
            .await
            .unwrap()
            .unwrap()
            .company
            .as_deref(),
        Some("Concurrent company")
    );
    let duplicate = browser
        .form_reply(
            &action,
            &[
                ("action", "save-organization-contacts"),
                ("company", "One"),
                ("company", "Two"),
            ],
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);
    let csrf = browser
        .exchange(
            "POST",
            &action,
            "application/x-www-form-urlencoded",
            b"csrf_token=incorrect&action=save-organization-contacts&company=No".to_vec(),
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    let saved = browser
        .form_reply(
            &action,
            &[
                ("action", "save-organization-contacts"),
                ("company", "Native contact"),
                ("email", "native-public@example.test"),
                ("location", "Montréal"),
                ("blog", "https://example.test/native"),
                ("twitter_username", "native"),
                ("billing_email", "native-private@example.test"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    let (_, public) = request(
        router,
        GET,
        "GET",
        "/api/v3/orgs/acme",
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(public["company"], "Native contact");
    assert_eq!(public["email"], "native-public@example.test");
    assert!(public.get("billing_email").is_none());
    let (status, public_page) = Client::anonymous(router).page("/acme").await;
    assert_eq!(status, StatusCode::OK);
    assert!(public_page.contains("Native contact"));
    assert!(public_page.contains("native-public@example.test"));
    assert!(!public_page.contains("native-private@example.test"));
    let policy = browser
        .form_reply(
            &action,
            &[
                ("action", "save-organization-security-policy"),
                ("web_commit_signoff_required", "true"),
            ],
        )
        .await;
    assert_eq!(policy.status, StatusCode::SEE_OTHER);
    let current = data::get(f.pool(), org).await.unwrap().unwrap();
    assert!(current.web_commit_signoff_required);
    assert!(
        !current.deploy_keys_enabled_for_repositories,
        "unchecked controls persist false"
    );
    assert_eq!(current.company.as_deref(), Some("Native contact"));
    let (_, document) = request(
        router,
        GET,
        "GET",
        "/api/v3/orgs/acme",
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(document["web_commit_signoff_required"], true);
    assert_eq!(document["deploy_keys_enabled_for_repositories"], false);
    assert_eq!(document["billing_email"], "native-private@example.test");
    let identity = first.id.to_string();
    let file_action = "/acme/first/_forms/write-file";
    let unsigned = browser
        .form_reply(
            file_action,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "web.txt"),
                ("expected_revision", "unborn"),
                ("creating", "yes"),
                ("message", "Web change"),
                ("content", "kept content"),
                ("intent", "save"),
            ],
        )
        .await;
    assert_eq!(unsigned.status, StatusCode::BAD_REQUEST);
    assert!(unsigned.text().contains("Signed-off-by"));
    assert!(unsigned.text().contains("kept content"));
    let user = db::users::accounts::get_user_by_id(f.pool(), owner)
        .await
        .unwrap()
        .unwrap();
    let trailer = format!("Signed-off-by: {} <{}>", user.username, user.commit_email());
    let signed = browser
        .form_reply(
            file_action,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "web.txt"),
                ("expected_revision", "unborn"),
                ("creating", "yes"),
                ("message", "Web change"),
                ("description", &trailer),
                ("content", "kept content"),
                ("intent", "save"),
            ],
        )
        .await;
    assert_eq!(signed.status, StatusCode::SEE_OTHER);
    // This setting describes web commits, not a Git/REST DCO rule.
    let (_, repo) = request(
        router,
        "repos/get",
        "GET",
        "/api/v3/repos/acme/first",
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(repo["id"], first.id);
    let (_, head) = request(
        router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/acme/first/git/ref/heads/main",
        write,
        Value::Null,
        200,
    )
    .await;
    let tip = head["object"]["sha"].as_str().unwrap();
    let api=browser.expect("POST","/api/repos/acme/first/contents/api.txt",json!({"content":"QVBJIGNvbnRlbnQ=","message":"API change without a sign-off","branch":"main","expected_revision":tip}),StatusCode::CREATED).await;
    let api_tip = api["sha"].as_str().unwrap().to_owned();
    let deletion = "/acme/first/_forms/delete-file";
    let unsigned = browser
        .form_reply(
            deletion,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "api.txt"),
                ("expected_revision", &api_tip),
                ("message", "Delete API file"),
            ],
        )
        .await;
    assert_eq!(unsigned.status, StatusCode::BAD_REQUEST);
    let message = format!("Delete API file\n\n{trailer}");
    let signed = browser
        .form_reply(
            deletion,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "api.txt"),
                ("expected_revision", &api_tip),
                ("message", &message),
            ],
        )
        .await;
    assert_eq!(signed.status, StatusCode::SEE_OTHER);
    let (_, main) = request(
        router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/acme/first/git/ref/heads/main",
        write,
        Value::Null,
        200,
    )
    .await;
    let main = main["object"]["sha"].as_str().unwrap();
    let unsigned = browser
        .form_reply(
            file_action,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "review.txt"),
                ("expected_revision", main),
                ("creating", "yes"),
                ("message", "Review change"),
                ("content", "Review content"),
                ("intent", "save"),
                ("commit_target", "new_branch"),
                ("new_branch", "review"),
            ],
        )
        .await;
    assert_eq!(unsigned.status, StatusCode::BAD_REQUEST);
    request(
        router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/acme/first/git/ref/heads/review",
        write,
        Value::Null,
        404,
    )
    .await;
    let signed = browser
        .form_reply(
            file_action,
            &[
                ("repository_id", &identity),
                ("branch", "main"),
                ("path", "review.txt"),
                ("expected_revision", main),
                ("creating", "yes"),
                ("message", "Review change"),
                ("description", &trailer),
                ("content", "Review content"),
                ("intent", "save"),
                ("commit_target", "new_branch"),
                ("new_branch", "review"),
            ],
        )
        .await;
    assert_eq!(signed.status, StatusCode::SEE_OTHER);
    let (_, unchanged) = request(
        router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/acme/first/git/ref/heads/main",
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(unchanged["object"]["sha"], main);
    request(
        router,
        "git/get-ref",
        "GET",
        "/api/v3/repos/acme/first/git/ref/heads/review",
        write,
        Value::Null,
        200,
    )
    .await;
}
