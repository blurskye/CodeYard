use super::github::{document, request};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use db::relationships::{Kind, Mutation};
use serde_json::{json, Value};

/// The stored hash of the Actions bot's job token in these tests.
const BOT_TOKEN_HASH: &str = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";

async fn parents(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (actor, name) in [(alice, "proj"), (alice, "other"), (bob, "foreign")] {
        repos.push(
            codeyard_app::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(actor),
                name,
                None,
                true,
                None,
            )
            .await
            .unwrap(),
        );
    }
    for repo in &repos[..2] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            bob,
            repo.id,
            codeyard_db_core::models::AccessLevel::Triage,
        )
        .await
        .unwrap();
    }
    let parent =
        codeyard_db_repos::issues::create_issue(f.pool(), repos[1].id, alice, "Parent", "")
            .await
            .unwrap();
    let foreign =
        codeyard_db_repos::issues::create_issue(f.pool(), repos[2].id, bob, "Foreign parent", "")
            .await
            .unwrap();
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), bob).await;
    let credential = tokens::create(
        f.pool(),
        bob,
        Some(&browser.session),
        TokenInput {
            name: "write",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let bearer = format!("Bearer {}", credential.value);
    let path = "/api/v3/repos/alice/proj/issues";
    let created = document(
        request(
            &router,
            "POST",
            path,
            Some(&bearer),
            json!({
                "title":"Created with a parent", "parent_issue_id":parent.id,
            }),
        )
        .await,
        "issues/create",
        201,
    )
    .await;
    let child = created["id"].as_i64().unwrap();
    let linked = document(
        request(
            &router,
            "GET",
            &format!("{path}/{}/parent", created["number"]),
            Some(&bearer),
            Value::Null,
        )
        .await,
        "issues/get-parent",
        200,
    )
    .await;
    assert_eq!(linked["id"], parent.id);
    assert_eq!(
        codeyard_db_identity::relationships::summaries(f.pool(), &[parent.id])
            .await
            .unwrap()
            .iter()
            .map(|row| row.total)
            .sum::<i64>(),
        1
    );

    let before = f
        .scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[0].id],
        )
        .await;
    for invalid in [Value::Null, json!("1"), json!(0), json!(-1), json!([])] {
        assert_eq!(
            request(
                &router,
                "POST",
                path,
                Some(&bearer),
                json!({"title":"Invalid", "parent_issue_id":invalid})
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        request(
            &router,
            "POST",
            path,
            Some(&bearer),
            json!({"title":"Wrong owner", "parent_issue_id":foreign.id})
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &router,
            "POST",
            path,
            Some(&bearer),
            json!({"title":"Missing parent", "parent_issue_id":99999999})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repos[1].id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    assert_eq!(
        request(
            &router,
            "POST",
            path,
            Some(&bearer),
            json!({"title":"Insufficient authority", "parent_issue_id":parent.id})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[0].id]
        )
        .await,
        before
    );

    // A service account can hold concurrent grants for separate jobs. Nested IDs must
    // not expand the repository context authorized by the request's job token.
    f.execute("INSERT INTO users(username,display_name,email_verified) VALUES ('github-actions[bot]','GitHub Actions',true)", &[]).await;
    let bot = f
        .scalar(
            "SELECT id FROM users WHERE username='github-actions[bot]'",
            &[],
        )
        .await;
    for repo in &repos[..2] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            f.pool(),
            bot,
            repo.id,
            codeyard_db_core::models::AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    assert!(codeyard_db_identity::relationships::list(
        f.pool(),
        Some(bot),
        child,
        Kind::Parent,
        0,
        100
    )
    .await
    .unwrap()
    .is_empty());
    let denied = codeyard_db_identity::relationships::change(
        f.pool(),
        bot,
        repos[0].id,
        child,
        Mutation::Add {
            kind: Kind::Related,
            other: parent.id,
            replace: false,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<codeyard_db_identity::relationships::Refusal>(),
        Some(codeyard_db_identity::relationships::Refusal::Missing)
    ));
    let denied = create_child(
        f.pool(),
        bot,
        repos[0].id,
        parent.id,
        "Cross-job parent",
        "",
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<codeyard_db_identity::relationships::Refusal>(),
        Some(codeyard_db_identity::relationships::Refusal::Missing)
    ));
    let local =
        codeyard_db_repos::issues::create_issue(f.pool(), repos[0].id, alice, "Local parent", "")
            .await
            .unwrap();
    let denied = codeyard_db_identity::relationships::change(
        f.pool(),
        bot,
        repos[0].id,
        child,
        Mutation::Add {
            kind: Kind::Parent,
            other: local.id,
            replace: true,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<codeyard_db_identity::relationships::Refusal>(),
        Some(codeyard_db_identity::relationships::Refusal::Missing)
    ));
    let same_repo = create_child(f.pool(), bot, repos[0].id, local.id, "Same-job parent", "")
        .await
        .unwrap();
    assert_eq!(
        codeyard_db_identity::relationships::list(
            f.pool(),
            Some(bot),
            same_repo.id,
            Kind::Parent,
            0,
            100
        )
        .await
        .unwrap()[0]
            .id,
        local.id
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[0].id]
        )
        .await,
        before + 2
    );

    // The bot acts with a job token; it cannot hold a browser session.
    f.execute(
        &format!("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope) VALUES ($1,'job','{BOT_TOKEN_HASH}','write')"),
        &[bot],
    )
    .await;
    let bot_token = f
        .scalar(
            "SELECT id FROM personal_access_tokens WHERE user_id=$1",
            &[bot],
        )
        .await;
    codeyard_db_repos::suggestions::set_enabled(
        f.pool(),
        repos[0].id,
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        true,
    )
    .await
    .unwrap();
    let proposal = codeyard_db_repos::suggestions::Proposal {
        action: codeyard_db_repos::suggestions::Action::CloseIssue,
        target_id: Some(parent.id),
        value: Value::Null,
        rationale: None,
        confidence: None,
    };
    let suggested = codeyard_db_repos::suggestions::propose(
        f.pool(),
        repos[0].id,
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(f.pool(), alice).await,
        ),
        child,
        &proposal,
        None,
    )
    .await
    .unwrap();
    for approve in [false, true] {
        let denied = codeyard_db_repos::suggestions::resolve(
            f.pool(),
            repos[0].id,
            bot,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Token(
                bot_token,
                BOT_TOKEN_HASH,
            ),
            child,
            suggested.id,
            approve,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            codeyard_db_repos::suggestions::refusal(&denied),
            Some(codeyard_db_repos::suggestions::Refusal::Missing)
        ));
    }
    let filter = codeyard_db_repos::suggestions::ListFilter {
        state: "all",
        action: None,
        own: false,
        descending: false,
        offset: 0,
        limit: 100,
    };
    assert!(
        codeyard_db_repos::suggestions::list(f.pool(), repos[0].id, bot, child, &filter)
            .await
            .unwrap()
            .is_empty()
    );
    let denied = codeyard_db_repos::suggestions::propose(
        f.pool(),
        repos[0].id,
        bot,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Token(
            bot_token,
            BOT_TOKEN_HASH,
        ),
        child,
        &proposal,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        codeyard_db_repos::suggestions::refusal(&denied),
        Some(codeyard_db_repos::suggestions::Refusal::Invalid)
    ));
    assert_eq!(
        codeyard_db_repos::suggestions::list(f.pool(), repos[0].id, alice, child, &filter)
            .await
            .unwrap()
            .len(),
        1
    );

    // Graph limits roll back the newly allocated issue as well as the edge.
    let mut deepest = local.id;
    for _ in 0..7 {
        deepest = create_child(f.pool(), alice, repos[0].id, deepest, "Deep child", "")
            .await
            .unwrap()
            .id;
    }
    let count = f
        .scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[0].id],
        )
        .await;
    let denied = create_child(f.pool(), alice, repos[0].id, deepest, "Too deep", "")
        .await
        .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<codeyard_db_identity::relationships::Refusal>(),
        Some(codeyard_db_identity::relationships::Refusal::Limit)
    ));
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1",
            &[repos[0].id]
        )
        .await,
        count
    );
    f.finish().await;
}
#[tokio::test]
async fn github_parent_creation_sqlite() {
    parents(false).await;
}
#[tokio::test]
async fn github_parent_creation_postgres() {
    parents(true).await;
}

/// A new issue under `parent`, created the way the API creates one.
async fn create_child(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo: i64,
    parent: i64,
    title: &str,
    body: &str,
) -> anyhow::Result<codeyard_db_core::models::Issue> {
    codeyard_db_repos::issues::create_issue_with_metadata(
        pool,
        repo,
        actor,
        title,
        body,
        codeyard_db_repos::issues::IssueMetadata {
            parent: Some(parent),
            ..Default::default()
        },
    )
    .await
}
