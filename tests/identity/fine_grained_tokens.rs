//! Fine-grained personal access tokens from the settings pages: the create form, the value
//! shown once, regeneration, expiry and deletion, on both engines.
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use codeyard::backend::{
    db::{
        self,
        organizations::{NewOrganization, OrganizationProfile},
        repositories::creation::RepositoryCreationOwner,
    },
    repositories::lifecycle,
};
use codeyard_webui::{
    app::Page,
    pages::{
        account,
        settings::fine_grained::{Model, View},
    },
};
use tower::ServiceExt;

const BASE: &str = "/settings/personal-access-tokens";

async fn model(client: &Client, path: &str) -> Model {
    let (status, page) = client.model(path).await;
    assert_eq!(status, 200, "{path}");
    let Page::Identity(account::Page::FineGrainedTokens { model }) = page else {
        panic!("{path} is not the fine-grained token page");
    };
    *model
}

/// The token value a page shows once.
fn value(html: &str) -> String {
    let start = html.find("cy_fgpat_").expect("a token value");
    html[start..start + 73].to_owned()
}

/// The status of an API request made with `token`, and whether it named OAuth scopes.
async fn api(router: &Router, path: &str, token: &str) -> (u16, bool) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("token {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let scopes = response.headers().contains_key("x-oauth-scopes");
    let status = response.status().as_u16();
    to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, scopes)
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let org = applied(
        db::organizations::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;

    // The form offers the user and their organizations, and the owner's repositories.
    let View::New { draft, choices } = model(&browser, &format!("{BASE}/new")).await.view else {
        panic!("the create form");
    };
    assert_eq!(draft.expiry, "30");
    assert!(choices.owners.contains(&("user".into(), "alice".into())));
    assert!(choices.owners.contains(&(org.to_string(), "acme".into())));
    assert_eq!(choices.repositories, vec![(repo.id, "alice/proj".into())]);
    let keys: Vec<_> = choices
        .permissions
        .iter()
        .map(|choice| choice.key.as_str())
        .collect();
    assert!(keys.contains(&"issues") && keys.contains(&"gists") && !keys.contains(&"members"));
    let html = browser.page_ok(&format!("{BASE}/new")).await;
    assert!(html.contains("method=\"post\"") && html.contains("name=\"permission.issues\""));

    // Switching the owner without JavaScript keeps the draft and offers that owner's permissions.
    let owner = org.to_string();
    let reply = browser
        .form_reply(
            BASE,
            &[
                ("action", "refresh"),
                ("name", "kept draft"),
                ("owner", &owner),
                ("expiry", "30"),
                ("access", "all"),
            ],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert!(reply.text().contains("kept draft"));
    assert!(reply.text().contains("name=\"permission.members\""));
    assert!(!reply.text().contains("name=\"permission.gists\""));

    // Refused forms keep what was typed.
    let id = repo.id.to_string();
    for (pairs, message) in [
        (
            vec![("access", "selected"), ("expiry", "30")],
            "Select 1-100 repositories",
        ),
        (
            vec![("access", "all"), ("expiry", "never")],
            "within the owner",
        ),
    ] {
        let mut form = vec![
            ("action", "create"),
            ("name", "refused draft"),
            ("owner", "user"),
        ];
        form.extend(pairs);
        let reply = browser.form_reply(BASE, &form).await;
        assert_eq!(reply.status, 400, "{}", reply.text());
        assert!(reply.text().contains("refused draft"));
        assert!(reply.text().contains(message), "{}", reply.text());
    }
    // Another user's repository cannot be selected.
    let foreign = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(bob),
        "foreign",
        None,
        true,
        None,
    )
    .await
    .unwrap()
    .id
    .to_string();
    let reply = browser
        .form_reply(
            BASE,
            &[
                ("action", "create"),
                ("name", "foreign"),
                ("owner", "user"),
                ("expiry", "30"),
                ("access", "selected"),
                ("repositories", &foreign),
            ],
        )
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());

    // Creating shows the value once.
    let reply = browser
        .form_reply(
            BASE,
            &[
                ("action", "create"),
                ("name", "deploy"),
                ("description", "CI deploys"),
                ("owner", "user"),
                ("expiry", "30"),
                ("access", "selected"),
                ("repositories", &id),
                ("permission.issues", "write"),
                ("permission.contents", "read"),
                ("permission.members", "read"),
            ],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let first = value(&reply.text());
    assert!(first[9..].chars().all(|c| c.is_ascii_hexdigit()));
    let row = db::fine_grained_tokens::list_before(f.pool(), alice, None)
        .await
        .unwrap()
        .remove(0);
    let token = row.token_id;
    assert_eq!(row.status, "approved");
    assert_eq!(row.repository_access, "selected");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.permissions).unwrap(),
        serde_json::json!({"issues":"write","contents":"read","metadata":"read"}),
        "account owners take no organization permissions"
    );
    assert_eq!(
        f.scalar(
            &format!(
                "SELECT COUNT(*) FROM personal_access_tokens WHERE id=$1 AND token_hash='{}'",
                codeyard::backend::identity::tokens::token_hash(&first)
            ),
            &[token]
        )
        .await,
        1,
        "only the hash is stored"
    );

    // gh authenticates with it: no OAuth scopes, the API root and the repository.
    assert_eq!(api(&router, "/api/v3/", &first).await, (200, false));
    assert_eq!(api(&router, "/api/v3/user", &first).await, (200, false));
    assert_eq!(
        api(&router, "/api/v3/repos/alice/proj/issues", &first)
            .await
            .0,
        200
    );

    // Its page never shows the value again, and lists what it grants.
    let View::Show {
        token: detail,
        value: shown,
    } = model(&browser, &format!("{BASE}/{token}")).await.view
    else {
        panic!("the token page");
    };
    assert_eq!(shown, None);
    assert!(!browser
        .page_ok(&format!("{BASE}/{token}"))
        .await
        .contains(&first));
    assert_eq!(detail.repositories, vec!["proj".to_owned()]);
    assert_eq!(detail.description, "CI deploys");
    assert!(detail
        .permissions
        .contains(&("Repository".into(), "Issues".into(), "write".into())));
    assert!(detail.row.last_used.is_some(), "use is recorded");
    let View::List {
        tokens,
        next_before,
    } = model(&browser, BASE).await.view
    else {
        panic!("the token list");
    };
    assert_eq!(tokens.len(), 1);
    assert_eq!(next_before, None, "one token fits on the first page");
    assert_eq!(
        (tokens[0].owner.as_str(), tokens[0].status.as_str()),
        ("alice", "approved")
    );
    // The classic list keeps classic tokens only, and says so.
    let (_, page) = browser.model("/settings?tab=tokens").await;
    let Page::Identity(account::Page::Settings {
        model: settings, ..
    }) = page
    else {
        panic!("the classic token settings");
    };
    assert!(settings.tokens.iter().all(|token| token.name != "deploy"));
    assert!(browser
        .page_ok("/settings?tab=tokens")
        .await
        .contains("Personal access tokens (classic)"));

    // Other users cannot see, regenerate or delete it.
    assert_eq!(other.page(&format!("{BASE}/{token}")).await.0, 404);
    let reply = other
        .form_reply(&format!("{BASE}/{token}/regenerate"), &[("expiry", "7")])
        .await;
    assert_eq!(reply.status, 404, "{}", reply.text());
    let reply = other
        .form_reply(&format!("{BASE}/{token}/delete"), &[("confirm", "yes")])
        .await;
    assert_eq!(reply.status, 404, "{}", reply.text());

    // Regenerating replaces the value; the old one stops working at once.
    let reply = browser
        .form_reply(&format!("{BASE}/{token}/regenerate"), &[("expiry", "7")])
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let second = value(&reply.text());
    assert_ne!(first, second);
    assert_eq!(api(&router, "/api/v3/user", &first).await.0, 401);
    assert_eq!(
        api(&router, "/api/v3/repos/alice/proj/issues", &second)
            .await
            .0,
        200
    );

    // An expired token is refused like a classic one.
    f.execute(
        "UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[token],
    )
    .await;
    assert_eq!(api(&router, "/api/v3/user", &second).await.0, 401);

    // Deleting needs confirmation, then removes it.
    let reply = browser
        .form_reply(&format!("{BASE}/{token}/delete"), &[])
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    let reply = browser
        .form_reply(&format!("{BASE}/{token}/delete"), &[("confirm", "yes")])
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    assert_eq!(
        reply.location.as_deref(),
        Some("/settings/personal-access-tokens?deleted=1")
    );
    assert!(db::fine_grained_tokens::get(f.pool(), token)
        .await
        .unwrap()
        .is_none());
    f.finish().await;
}

#[tokio::test]
async fn sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn postgres() {
    contract(true).await;
}
