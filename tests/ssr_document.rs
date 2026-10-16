//! SSR document and safe bootstrap contracts using typed view fixtures.
use codeyard::webui::{
    app::{Page, PageEnvelope},
    bootstrap::{Bootstrap, BOOTSTRAP_ID},
    document::{self, Chrome},
    pages::repositories::{self, Editor, RepoChrome},
};

#[test]
fn initial_document_embeds_the_exact_safe_navigation_model() {
    let location = "/search?q=a%26b&tag=one&tag=two&empty=";
    let message = "</script><script>alert('fixture')</script>&draft";
    let envelope = PageEnvelope {
        title: "Request failed".into(),
        location: location.into(),
        chrome: Chrome {
            csrf_token: "a".repeat(64),
            return_to: location.into(),
            ..Default::default()
        },
        page: Page::Error {
            status: 409,
            message: message.into(),
            repo: None,
            signed_in: false,
        },
    };
    let html = document::render_model(envelope, "/assets/fixture.js").unwrap();
    assert!(html.starts_with("<!DOCTYPE html><html"));
    assert!(!html.contains("<script>alert('fixture')</script>"));
    let bootstrap = html
        .split(&format!("id=\"{BOOTSTRAP_ID}\""))
        .nth(1)
        .unwrap()
        .split_once('>')
        .unwrap()
        .1
        .split_once("</script>")
        .unwrap()
        .0;
    let bootstrap: Bootstrap = serde_json::from_str(bootstrap).unwrap();
    assert_eq!(bootstrap.protocol, 1);
    assert_eq!(bootstrap.initial.location, location);
    assert_eq!(bootstrap.initial.chrome.return_to, location);
    assert_eq!(bootstrap.initial.chrome.csrf_token, "a".repeat(64));
    assert!(
        matches!(bootstrap.initial.page, Page::Error { status: 409, message: actual, .. } if actual == message)
    );
}

#[test]
fn patch_rows_preserve_context_headers_prefixes_and_unterminated_text() {
    let html = document::render("Patch", Chrome::default(), codeyard::webui::components::patch_lines(
        "--- a/file\n+++ b/file\n@@ -1,2 +1,2 @@\n context\n-old\n+<script>new</script>\n\\ No newline at end of file",
    ));
    // The file headers are not rows: the page shows the file's own header instead.
    assert!(!html.contains("--- a/file"), "{html}");
    assert!(!html.contains("+++ b/file"), "{html}");
    // Context and changed lines carry their old and new line numbers as attributes.
    for (attributes, text) in [
        ("class=\"patch-line patch-hunk\"", "@@ -1,2 +1,2 @@\n"),
        (
            "class=\"patch-line\" data-old=\"1\" data-new=\"1\"",
            " context\n",
        ),
        (
            "class=\"patch-line patch-deletion\" data-old=\"2\"",
            "-old\n",
        ),
        (
            "class=\"patch-line patch-addition\" data-new=\"2\"",
            "+&#60;script&#62;new&#60;/script&#62;\n",
        ),
        ("class=\"patch-line\"", "\\ No newline at end of file"),
    ] {
        assert!(
            html.contains(&format!("<span {attributes}>{text}</span>")),
            "{html}"
        );
    }
    assert!(!html.contains("<script>new</script>"));
}

#[test]
fn editor_initial_document_has_native_fallback_controls_and_safe_text() {
    let token = "a".repeat(64);
    let repo = RepoChrome {
        owner: "fixture-owner".into(),
        name: "fixture-repo".into(),
        signed_in: true,
        can_write: true,
        csrf_token: token.clone(),
        current_path: "/fixture-owner/fixture-repo/edit/main/file.md".into(),
        ..Default::default()
    };
    let editor = Editor {
        branch: "main".into(),
        path: "file.md".into(),
        content: "</textarea><script>alert('fixture')</script> & preserved text".into(),
        message: "fixture commit".into(),
        expected_revision: "b".repeat(40),
        ..Default::default()
    };
    let html = document::render(
        "Editor fixture",
        Chrome {
            username: Some(repo.owner.clone()),
            csrf_token: token,
            return_to: repo.current_path.clone(),
            ..Default::default()
        },
        repositories::editor(&repo, &editor),
    );
    assert!(html.starts_with("<!DOCTYPE html><html"));
    assert!(html.ends_with("</html>"));
    // A GET at the form URL (a reload of a rejected save) returns to this editor.
    assert!(html.contains(
        "action=\"/fixture-owner/fixture-repo/_forms/write-file?from=%2Ffixture-owner%2Ffixture-repo%2Fedit%2Fmain%2Ffile.md\""
    ));
    assert!(html.contains("name=\"expected_revision\""));
    assert!(html.contains("name=\"content\""));
    assert!(html.contains("&lt;/textarea&gt;&lt;script&gt;"));
    // Generated Dioxus hydration is allowed by the latest user correction.
    // Submitted editor text must never become an executable script, with or without it.
    assert!(!html.contains("<script>alert('fixture')</script>"));
    for form in html.split("<form").skip(1) {
        let form = form.split("</form>").next().expect("closed native form");
        assert!(form.contains("type=\"submit\""));
        if form.contains("method=\"post\"") {
            assert!(form.contains("name=\"csrf_token\""));
        } else {
            assert!(
                form.contains("method=\"get\""),
                "unexpected native form method"
            );
        }
    }
    // Optional generated evidence belongs only to the validator-selected artifact path.
    if let Ok(path) = std::env::var("SSR_PROBE_OUTPUT") {
        std::fs::write(path, html).expect("save static renderer evidence");
    }
}
