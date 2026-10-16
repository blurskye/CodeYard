use crate::fixture::Fixture;
use axum::http::StatusCode;
use codeyard::webui::pages::organizations::{self, Page};
use dioxus::prelude::*;

fn root(page: Page) -> Element {
    organizations::render(page)
}
pub fn html(page: Page) -> String {
    let mut dom = VirtualDom::new_with_props(root, page);
    dom.rebuild_in_place();
    let mut renderer = dioxus_ssr::Renderer::new();
    renderer.pre_render = true;
    renderer.render(&dom)
}
fn profile_form(html: &str) -> &str {
    html.split("<form")
        .find(|form| form.contains("value=\"profile\""))
        .expect("real profile native form")
        .split_once("</form>")
        .unwrap()
        .0
}

pub async fn contracts(f: &Fixture) {
    for path in [crate::fixture::INDEX.to_owned(), f.path()] {
        let page = f.get(&f.owner, &path).await.organization();
        let org = page
            .selected
            .as_ref()
            .or_else(|| page.organizations.iter().find(|org| org.id == f.org))
            .unwrap();
        assert_eq!(org.name, "native_org");
        assert_eq!(org.display_name.as_deref(), Some("Stored display"));
        let response = f
            .request(
                &f.owner,
                "GET",
                &path,
                String::new(),
                "application/x-www-form-urlencoded",
                (false, false),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains("@native_org"));
        assert!(response.body.contains("Stored display"));
        assert!(!response.body.contains("@Stored display"));
    }
    let page = f.get(&f.owner, &f.path()).await.organization();
    let source = html(page);
    let form = profile_form(&source);
    assert_eq!(
        source.matches("method=\"post\"").count(),
        source.matches("name=\"csrf_token\"").count()
    );
    assert!(source.contains("value=\"add-member\""));
    assert!(source.contains("value=\"member-role\""));
    assert!(!source.contains("action=\"/orgs"));
    assert!(form.contains("value=\"Stored display\""));
    assert!(form.contains("name=\"description\""));
    assert!(form.contains("<textarea"));
    assert!(form.contains("Stored description\nSecond line"));
    let untouched = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "profile"),
                ("display_name", "Stored display"),
                ("description", "Stored description\nSecond line"),
            ],
        )
        .await;
    untouched.redirect(&f.path());
    let page = f.get(&f.owner, &f.path()).await.organization();
    let org = page.selected.unwrap();
    assert_eq!(org.display_name.as_deref(), Some("Stored display"));
    assert_eq!(org.description, "Stored description\nSecond line");
    let xss = "\n</textarea><script>alert(1)</script>&\nLast line";
    f.form(
        &f.owner,
        &f.path(),
        &[("action", "profile"), ("description", xss)],
    )
    .await
    .redirect(&f.path());
    let org = f
        .get(&f.owner, &f.path())
        .await
        .organization()
        .selected
        .unwrap();
    assert_eq!(
        org.display_name.as_deref(),
        Some("Stored display"),
        "missing field must survive partial edits"
    );
    assert_eq!(org.description, xss);
    f.form(
        &f.owner,
        &f.path(),
        &[("action", "profile"), ("display_name", "New display")],
    )
    .await
    .redirect(&f.path());
    let page = f.get(&f.owner, &f.path()).await.organization();
    let saved = page.selected.as_ref().unwrap();
    assert_eq!(saved.description, xss);
    assert_eq!(saved.display_name.as_deref(), Some("New display"));
    let source = html(page);
    let form = profile_form(&source);
    assert!(form.contains("value=\"New display\""), "{form}");
    assert!(
        form.contains("\n\n&lt;/textarea&gt;&lt;script&gt;alert(1)&lt;/script&gt;&amp;\nLast line")
    );
    assert!(!form.contains("<script>"));
    let textarea = form
        .split_once("<textarea")
        .unwrap()
        .1
        .split_once('>')
        .unwrap()
        .1
        .split_once("</textarea>")
        .unwrap()
        .0;
    assert!(
        !textarea.contains("<!--"),
        "opaque textarea text must not include hydration node markers"
    );

    // Optional display_name is not the slug/heading; explicit blanks clear it.
    f.form(
        &f.owner,
        &f.path(),
        &[("action", "profile"), ("display_name", "")],
    )
    .await
    .redirect(&f.path());
    let page = f.get(&f.owner, &f.path()).await.organization();
    assert_eq!(page.selected.as_ref().unwrap().display_name, None);
    assert_eq!(page.selected.as_ref().unwrap().name, "native_org");
    let source = html(page);
    assert!(!profile_form(&source).contains("value=\"native_org\""));
    let bad = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "profile"),
                ("display_name", "   "),
                ("description", xss),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    bad.secure();
    assert_eq!(bad.organization().draft["description"], xss);
    let source = html(bad.organization());
    assert!(!profile_form(&source).contains("<script>"));
    // Known bounded values survive errors; unknown/secret/oversized fields do not.
    let oversized = "x".repeat(4097);
    let bad = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "profile"),
                ("display_name", "Bounded draft"),
                ("description", &oversized),
                ("billing_email", "private@example.test"),
                ("password", "secret-not-for-model"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let draft = bad.organization().draft;
    assert_eq!(draft["display_name"], "Bounded draft");
    for excluded in ["description", "billing_email", "password", "csrf_token"] {
        assert!(!draft.contains_key(excluded));
    }
    assert!(!bad.body.contains("secret-not-for-model"));
    assert!(!bad.body.contains("private@example.test"));
    let unknown = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "secret-unknown-command"),
                ("description", "unknown payload"),
            ],
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(unknown.organization().draft.is_empty());
    // Safe models have neither backend records nor billing/credential fields.
    let model = serde_json::to_value(f.get(&f.owner, &f.path()).await.organization()).unwrap();
    let selected = model["selected"].as_object().unwrap();
    for key in ["billing_email", "password_hash", "pool", "actor", "backend"] {
        assert!(!selected.contains_key(key));
    }
}
