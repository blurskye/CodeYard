//! Branch protection rules on the repository settings page: the native forms add, change and
//! delete rules, refuse bad input with the submitted values kept, and show exactly what the JSON
//! API stores. On SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;

use axum::http::StatusCode;
use codeyard::backend::db;
use database::Database;
use pulls::Client;
use serde_json::{json, Value};

/// The rendered form that posts to `action` and shows `message`.
fn form_with_message<'a>(page: &'a str, action: &str, message: &str) -> &'a str {
    assert!(page.contains(message), "{page}");
    page.split("<form")
        .skip(1)
        .filter_map(|form| form.split("</form>").next())
        .find(|form| form.contains(&format!("action=\"{action}\"")) && form.contains(message))
        .expect("the refused form, showing its message")
}

/// The settings tab that a rendered settings page marks as current.
fn current_tab(page: &str) -> &str {
    let link = page
        .split("<a ")
        .find(|link| {
            let tag = link.split('>').next().unwrap_or_default();
            tag.contains("aria-current=\"page\"") && tag.contains("tab=")
        })
        .expect("a settings tab marked as current");
    link.split("tab=")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| {
            panic!(
                "the current tab's name in {:?}",
                &link[..link.len().min(300)]
            )
        })
}

/// The value the rendered text input `name` carries. Only the quote escape is undone, which is the
/// one escape the status check names below need.
fn input_value(form: &str, name: &str) -> String {
    let input = form
        .split("<input")
        .find(|input| input.contains(&format!("name=\"{name}\"")))
        .expect("the input, rendered");
    input
        .split("value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the input's value")
        .replace("&#34;", "\"")
}

async fn contract(pg: bool) {
    // The native forms answer with rendered pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    // Carol has no access to the repository, so she cannot be a push allowance.
    f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/collaborators",
            json!({"username": "bob", "access_level": "write"}),
            StatusCode::OK,
        )
        .await;
    let proj = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    let repo_id = proj["id"].as_i64().unwrap();
    let repo = repo_id.to_string();
    let branches = "/alice/proj/settings?tab=branches";
    let save = "/alice/proj/_forms/protection-save";
    let remove = "/alice/proj/_forms/protection-delete";
    let rules_url = "/api/repos/alice/proj/branch-protection";

    // Only administrators see the section. A writer is refused the page and both forms.
    let (status, page) = alice.page(branches).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("No branch protection rules have been added."),
        "{page}"
    );
    let (status, page) = bob.page(branches).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("required_approving_reviews", "1"),
                ("block_force_pushes", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = bob
        .form(
            remove,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(
        alice
            .expect("GET", rules_url, Value::Null, StatusCode::OK)
            .await,
        json!([])
    );

    // The add form saves a rule that the JSON API returns field for field.
    let add = [
        ("repository_id", repo.as_str()),
        ("pattern", "main"),
        ("required_approving_reviews", "2"),
        ("dismiss_stale_reviews", "on"),
        ("require_pull_request", "on"),
        ("require_status_checks", "on"),
        ("required_status_checks", "lint, test, ci/build"),
        ("require_up_to_date_branch", "on"),
        ("require_linear_history", "on"),
        ("require_conversation_resolution", "on"),
        ("block_force_pushes", "on"),
        ("block_deletions", "on"),
        ("restrict_pushes", "on"),
        ("push_allowances", "bob, bob"),
        ("force_push_allowances", "bob"),
        ("enforce_admins", "on"),
    ];
    let reply = alice.form_reply(save, &add).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some(branches));
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    let main = &rules[0];
    assert_eq!(main["pattern"], "main", "{main}");
    assert_eq!(main["required_approving_reviews"], 2, "{main}");
    assert_eq!(main["dismiss_stale_reviews"], true, "{main}");
    assert_eq!(main["require_status_checks"], true, "{main}");
    assert_eq!(
        main["required_status_checks"],
        json!(["lint", "test", "ci/build"]),
        "{main}"
    );
    assert_eq!(main["require_up_to_date_branch"], true, "{main}");
    assert_eq!(main["require_linear_history"], true, "{main}");
    assert_eq!(main["require_conversation_resolution"], true, "{main}");
    assert_eq!(main["allow_force_pushes"], false, "{main}");
    assert_eq!(main["allow_deletions"], false, "{main}");
    assert_eq!(main["restrict_pushes"], true, "{main}");
    assert_eq!(main["push_allowances"], json!(["bob"]), "{main}");
    assert_eq!(main["force_push_allowances"], json!(["bob"]), "{main}");
    assert_eq!(main["enforce_admins"], true, "{main}");
    let (status, page) = alice.page(branches).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("2 approvals required"), "{page}");
    assert!(page.contains("Force pushes allowed for bob"), "{page}");
    assert!(
        page.contains("Status checks: lint, test, ci/build"),
        "{page}"
    );
    assert!(page.contains("Pushes limited to bob"), "{page}");

    // The edit form posts the pattern that names the rule, so saving it changes that rule.
    let edit = [
        ("repository_id", repo.as_str()),
        ("pattern", "main"),
        ("required_approving_reviews", "1"),
        ("dismiss_stale_reviews", "on"),
        ("require_pull_request", "on"),
        ("require_linear_history", "on"),
        ("block_deletions", "on"),
        ("restrict_pushes", "on"),
        ("push_allowances", "bob"),
        ("force_push_allowances", "bob"),
        ("enforce_admins", "on"),
    ];
    let reply = alice.form_reply(save, &edit).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    let main = &rules[0];
    assert_eq!(main["required_approving_reviews"], 1, "{main}");
    assert_eq!(main["require_status_checks"], false, "{main}");
    assert_eq!(main["required_status_checks"], json!([]), "{main}");
    assert_eq!(main["require_conversation_resolution"], false, "{main}");
    assert_eq!(main["allow_force_pushes"], true, "{main}");
    assert_eq!(main["push_allowances"], json!(["bob"]), "{main}");
    assert_eq!(main["force_push_allowances"], json!(["bob"]), "{main}");

    // A refused add comes back with what was typed, next to the add form, and saves nothing.
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "bad..name"),
                ("required_approving_reviews", "2"),
                ("require_pull_request", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
                ("push_allowances", "bob, carol"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let refused = form_with_message(
        &page,
        save,
        "Use a branch name or a pattern with *, ** or ?",
    );
    assert_eq!(current_tab(&page), "branches", "{page}");
    assert!(refused.contains("value=\"bad..name\""), "{refused}");
    assert!(refused.contains("value=\"2\""), "{refused}");
    assert!(refused.contains("bob, carol"), "{refused}");

    // A push allowance that is not a collaborator is refused with the same message the API gives.
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "release/**"),
                ("required_approving_reviews", "0"),
                ("require_pull_request", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
                ("restrict_pushes", "on"),
                ("push_allowances", "bob carol"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let refused = form_with_message(
        &page,
        save,
        "push_allowances must name users with write access to this repository",
    );
    assert!(refused.contains("release/**"), "{refused}");
    assert!(refused.contains("bob, carol"), "{refused}");
    let (status, body) = alice
        .api(
            "PUT",
            rules_url,
            json!({"pattern": "release/**", "push_allowances": ["bob", "carol"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "validation_error", "{body}");
    assert_eq!(
        body["message"], "push_allowances must name users with write access to this repository",
        "{body}"
    );

    // Force push allowances need write access too, and the list is bounded like the push list.
    let (status, body) = alice
        .api(
            "PUT",
            rules_url,
            json!({"pattern": "release/**", "force_push_allowances": ["bob", "carol"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["message"],
        "force_push_allowances must name users with write access to this repository",
        "{body}"
    );
    let (status, body) = alice
        .api(
            "PUT",
            rules_url,
            json!({"pattern": "release/**", "force_push_allowances": vec!["bob"; 101]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["message"], "force_push_allowances can list at most 100 entries",
        "{body}"
    );

    // The picker posts each checked person as a repeated field and the typed names as one field.
    // Both reach the rule in order, once each. The page offers the viewer first.
    // A differently cased, nonexistent account is not an alias for bob.
    let case_refused = alice
        .form_reply(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("required_approving_reviews", "1"),
                ("force_push_allowances", "bob"),
                ("force_push_extra", "BOB, alice"),
            ],
        )
        .await;
    assert_eq!(
        case_refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        case_refused.text()
    );
    let mut picked = [
        ("repository_id", repo.as_str()),
        ("pattern", "main"),
        ("required_approving_reviews", "1"),
        ("require_pull_request", "on"),
        ("block_force_pushes", "on"),
        ("block_deletions", "on"),
        ("enforce_admins", "on"),
        ("force_push_allowances", "bob"),
        ("force_push_extra", "BOB, alice"),
    ];
    // A differently cased name cannot silently select the lowercase account.
    let refused = alice.form_reply(save, &picked).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        refused.text()
    );
    picked.last_mut().unwrap().1 = "bob, alice";
    let reply = alice.form_reply(save, &picked).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(
        rules[0]["force_push_allowances"],
        json!(["bob", "alice"]),
        "{rules}"
    );
    let (status, page) = alice.page(branches).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("You (@alice)"), "{page}");

    // A checked person without write access is refused with the message the API gives.
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("required_approving_reviews", "1"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
                ("force_push_allowances", "carol"),
                ("force_push_allowances", "alice"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let refused = form_with_message(
        &page,
        save,
        "force_push_allowances must name users with write access to this repository",
    );
    assert!(refused.contains("value=\"carol\""), "{refused}");
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(
        rules[0]["force_push_allowances"],
        json!(["bob", "alice"]),
        "{rules}"
    );

    // A refused edit of a saved rule opens that rule's form with the value that was refused.
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("required_approving_reviews", "two"),
                ("require_pull_request", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let refused = form_with_message(&page, save, "Required approvals must be a whole number.");
    assert!(refused.contains("value=\"main\""), "{refused}");
    assert!(refused.contains("value=\"two\""), "{refused}");
    // The approvals field is text, so the draft that a number input would blank is kept.
    assert!(
        refused.split("<input").any(|input| {
            input.contains("name=\"required_approving_reviews\"")
                && input.contains("type=\"text\"")
                && input.contains("value=\"two\"")
        }),
        "{refused}"
    );
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    assert_eq!(rules[0]["required_approving_reviews"], 1, "{rules}");

    // A form still open from before a rename or delete names the repository by ID. A stale ID is
    // refused before anything is saved or deleted, as the other settings forms refuse it.
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", "0"),
                ("pattern", "stale-rule"),
                ("required_approving_reviews", "0"),
                ("require_pull_request", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("The repository changed"), "{page}");
    let (status, page) = alice
        .form(
            remove,
            &[
                ("repository_id", "0"),
                ("pattern", "main"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("The repository changed"), "{page}");
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    assert_eq!(rules[0]["pattern"], "main", "{rules}");

    // Deleting asks for confirmation first, then removes the rule for the API too.
    let (status, page) = alice
        .form(
            remove,
            &[("repository_id", repo.as_str()), ("pattern", "main")],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(
        page.contains("Explicit confirmation is required."),
        "{page}"
    );
    let reply = alice
        .form_reply(
            remove,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(
        alice
            .expect("GET", rules_url, Value::Null, StatusCode::OK)
            .await,
        json!([])
    );
    // A rule that is already gone is reported at the top of the page.
    let (status, page) = alice
        .form(
            remove,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
    assert!(page.contains("Settings were not saved"), "{page}");
    assert_eq!(current_tab(&page), "branches", "{page}");
    assert!(page.contains("Branch protection rule not found"), "{page}");

    // A rule saved through the JSON API is listed on the page, and deleting it there removes it.
    let put = alice
        .expect(
            "PUT",
            rules_url,
            json!({"pattern": "hotfix/*", "require_status_checks": true, "required_status_checks": ["ci"]}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(put["pattern"], "hotfix/*", "{put}");
    let (status, page) = alice.page(branches).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("hotfix/*"), "{page}");
    assert!(page.contains("Status checks: ci"), "{page}");
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branch-protection/hotfix%2F*",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    let page = alice.page_ok(branches).await;
    assert!(!page.contains("Status checks: ci"), "{page}");

    // A check name with a comma is quoted in the form. Saving the edit form with that value
    // unchanged keeps the same two names.
    let put = alice
        .expect(
            "PUT",
            rules_url,
            json!({"pattern": "checks/*", "require_status_checks": true, "required_status_checks": ["build, linux", "ci / test"]}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        put["required_status_checks"],
        json!(["build, linux", "ci / test"]),
        "{put}"
    );
    let (status, page) = alice.page(branches).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let edit = form_with_message(&page, save, "value=\"checks/*\"");
    let names = input_value(edit, "required_status_checks");
    assert_eq!(names, "\"build, linux\", ci / test", "{edit}");
    let reply = alice
        .form_reply(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "checks/*"),
                ("required_approving_reviews", "0"),
                ("require_pull_request", "on"),
                ("dismiss_stale_reviews", "on"),
                ("require_status_checks", "on"),
                ("required_status_checks", names.as_str()),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let rules = alice
        .expect("GET", rules_url, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(rules.as_array().unwrap().len(), 1, "{rules}");
    assert_eq!(
        rules[0]["required_status_checks"],
        json!(["build, linux", "ci / test"]),
        "{rules}"
    );

    // A check name must fit a single-line field, so a line break or tab is refused through the API
    // and through the form. The form shows the refused draft again with the message.
    for name in ["build\nlinux", "build\tlinux"] {
        let (status, body) = alice
            .api(
                "PUT",
                rules_url,
                json!({"pattern": "names", "require_status_checks": true, "required_status_checks": [name]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "validation_error", "{body}");
        assert_eq!(
            body["message"], "required_status_checks entries cannot contain control characters",
            "{body}"
        );
    }
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "names"),
                ("required_approving_reviews", "0"),
                ("require_pull_request", "on"),
                ("require_status_checks", "on"),
                ("required_status_checks", "build\nlinux"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let refused = form_with_message(
        &page,
        save,
        "required_status_checks entries cannot contain control characters",
    );
    assert!(refused.contains("value=\"names\""), "{refused}");
    // Ordinary names, a name with a comma and a name with quotes are kept as given.
    let saved = alice
        .expect(
            "PUT",
            rules_url,
            json!({"pattern": "names", "require_status_checks": true, "required_status_checks": ["ci / test", "build, linux", "say \"hi\""]}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        saved["required_status_checks"],
        json!(["ci / test", "build, linux", "say \"hi\""]),
        "{saved}"
    );

    // An archived repository refuses the change in both paths, with the same reason.
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[repo_id],
    )
    .await;
    let (status, body) = alice
        .api("PUT", rules_url, json!({"pattern": "main"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "forbidden", "{body}");
    let (status, page) = alice
        .form(
            save,
            &[
                ("repository_id", repo.as_str()),
                ("pattern", "main"),
                ("required_approving_reviews", "0"),
                ("require_pull_request", "on"),
                ("block_force_pushes", "on"),
                ("block_deletions", "on"),
                ("enforce_admins", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert!(
        page.contains("Repository policy authority is denied"),
        "{page}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_branch_protection_settings_match_the_api() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_branch_protection_settings_match_the_api() {
    contract(true).await;
}
