//! Repository webhooks through the settings forms and the API, on SQLite and PostgreSQL: who may
//! manage hooks, that the secret is never rendered, the signed request a local receiver gets,
//! event filtering, retries, and redelivery. The receiver listens on 127.0.0.1, so this test
//! turns on `WEBHOOKS_ALLOW_PRIVATE` for the whole process, after the checks that need it off.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    crypto::SecretEncryptor,
    db::{
        self,
        models::OrgRole,
        organizations::{self as orgs, OrganizationOutcome},
        webhooks::{HookOwner, WebhookFields},
    },
    storage::ObjectStore,
    webhooks::delivery::hmac_sha256,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SECRET: &str = "s3cret-value";

/// What the local receiver saw: lower-case header names and the body.
#[derive(Clone, Debug)]
struct Received {
    headers: Vec<(String, String)>,
    body: String,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Default)]
struct Receiver {
    requests: Arc<Mutex<Vec<Received>>>,
    /// Statuses for the next requests; 200 once this runs out.
    replies: Arc<Mutex<VecDeque<u16>>>,
}

impl Receiver {
    /// Listens on an ephemeral 127.0.0.1 port and returns the URL to post to.
    async fn start(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        raw.extend_from_slice(&chunk[..read]);
                    };
                    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
                    let headers: Vec<(String, String)> = head
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| {
                            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect();
                    let length: usize = headers
                        .iter()
                        .find(|(name, _)| name == "content-length")
                        .and_then(|(_, value)| value.parse().ok())
                        .unwrap_or(0);
                    while raw.len() < header_end + length {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        raw.extend_from_slice(&chunk[..read]);
                    }
                    let body = String::from_utf8_lossy(&raw[header_end..]).into_owned();
                    state
                        .requests
                        .lock()
                        .unwrap()
                        .push(Received { headers, body });
                    let status = state.replies.lock().unwrap().pop_front().unwrap_or(200);
                    let response = format!(
                        "HTTP/1.1 {status} Status\r\nContent-Length: 2\r\nX-Receiver: yes\r\nConnection: close\r\n\r\nok"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}/hook")
    }

    fn received(&self) -> Vec<Received> {
        self.requests.lock().unwrap().clone()
    }
}

/// `alice/proj` is public and bob can read it. Alice owns it.
async fn setup(pg: bool) -> (Database, tempfile::TempDir, Client, Client) {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (database.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(Arc::new(SecretEncryptor::new(&[42; 32]))),
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    (database, root, alice, bob)
}

fn save_pairs<'a>(repo: &'a str, url: &'a str, events: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut pairs = vec![
        ("repository_id", repo),
        ("payload_url", url),
        ("content_type", "application/json"),
        ("secret", SECRET),
        ("active", "yes"),
        ("ssl_verify", "yes"),
    ];
    pairs.extend(events.iter().map(|event| ("events", *event)));
    pairs
}

/// Waits until the hook has at least `count` recorded deliveries, newest first.
async fn deliveries(
    database: &Database,
    hook: i64,
    count: usize,
) -> Vec<db::models::WebhookDelivery> {
    for _ in 0..200 {
        let rows = db::webhooks::list_deliveries(database.pool(), hook)
            .await
            .unwrap();
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("webhook {hook} never reached {count} deliveries");
}

async fn repo_id(database: &Database) -> String {
    db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap()
        .id
        .to_string()
}

async fn hook_id(database: &Database) -> i64 {
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let hooks = db::webhooks::list_webhooks(database.pool(), HookOwner::Repository(repo.id))
        .await
        .unwrap();
    assert_eq!(hooks.len(), 1, "exactly one hook");
    hooks[0].id
}

/// Before the receiver is allowed: a private address is refused when a hook is saved, and a
/// reader cannot save one at all.
async fn refusals(pg: bool) {
    let (database, _root, alice, bob) = setup(pg).await;
    let repo = repo_id(&database).await;
    let (status, text) = alice
        .form(
            "/alice/proj/_forms/webhook-save",
            &save_pairs(&repo, "http://127.0.0.1:9/hook", &["push"]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("Payload URL is not allowed"), "{text}");
    let (status, _) = bob
        .form(
            "/alice/proj/_forms/webhook-save",
            &save_pairs(&repo, "https://93.184.216.34/hook", &["push"]),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn flow(pg: bool, receiver: &Receiver, url: &str) {
    let (database, _root, alice, bob) = setup(pg).await;
    let repo = repo_id(&database).await;

    // Saving a hook queues a ping; the receiver sees it signed with the secret.
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/webhook-save",
            &save_pairs(&repo, url, &["issues"]),
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/alice/proj/settings?tab=webhooks&notice=saved")
    );
    let hook = hook_id(&database).await;
    let rows = deliveries(&database, hook, 1).await;
    assert_eq!(rows[0].event, "ping");
    assert_eq!(rows[0].status_code, Some(200));
    let ping = receiver.received().remove(0);
    assert_eq!(ping.header("x-github-event"), Some("ping"));
    assert_eq!(
        ping.header("x-github-delivery"),
        Some(rows[0].guid.as_str())
    );
    assert!(ping
        .header("user-agent")
        .unwrap()
        .starts_with("Codeyard-Hookshot/"));
    let expected = format!(
        "sha256={}",
        hex::encode(hmac_sha256(SECRET.as_bytes(), ping.body.as_bytes()))
    );
    assert_eq!(ping.header("x-hub-signature-256"), Some(expected.as_str()));
    let body: Value = serde_json::from_str(&ping.body).unwrap();
    assert!(body["zen"].is_string());
    assert_eq!(body["repository"]["full_name"], json!("alice/proj"));

    // The settings page lists the hook and never renders the secret.
    let (status, page) = alice.page("/alice/proj/settings?tab=webhooks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(url), "hook URL is listed");
    assert!(page.contains("Webhooks"));
    assert!(!page.contains(SECRET), "the secret is never rendered");
    let (_, edit) = alice
        .page(&format!(
            "/alice/proj/settings?tab=webhooks&hook={hook}&edit=1"
        ))
        .await;
    assert!(edit.contains("Leave blank to keep the saved secret"));
    assert!(!edit.contains(SECRET));
    let (status, _) = bob.page("/alice/proj/settings?tab=webhooks").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "readers cannot open the section"
    );

    // Only subscribed events are sent: an issue is, and a comment on it is not.
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Hooked", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let rows = deliveries(&database, hook, 2).await;
    assert_eq!(rows[0].event, "issues");
    assert_eq!(rows[0].action.as_deref(), Some("opened"));
    let issue = receiver.received().pop().unwrap();
    assert_eq!(issue.header("x-github-event"), Some("issues"));
    let body: Value = serde_json::from_str(&issue.body).unwrap();
    assert_eq!(body["action"], json!("opened"));
    assert_eq!(body["issue"]["title"], json!("Hooked"));
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues/1/comments",
            json!({"body": "Not subscribed"}),
            StatusCode::CREATED,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        db::webhooks::list_deliveries(database.pool(), hook)
            .await
            .unwrap()
            .len(),
        2,
        "a comment is not sent to a hook that subscribes to issues only"
    );

    // Redelivery sends the recorded body again, under a new delivery id.
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/webhook-redeliver",
            &[
                ("repository_id", &repo),
                ("hook_id", &hook.to_string()),
                ("delivery_id", &rows[0].id.to_string()),
            ],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let rows = deliveries(&database, hook, 3).await;
    assert_eq!(rows[0].event, "issues");
    assert_eq!(rows[0].request_body, rows[1].request_body);
    assert_ne!(rows[0].guid, rows[1].guid);

    // Server errors are retried: two 500s, then a 200 on the third try.
    receiver.replies.lock().unwrap().extend([500, 500]);
    alice
        .form_reply(
            "/alice/proj/_forms/webhook-ping",
            &[("repository_id", &repo), ("hook_id", &hook.to_string())],
        )
        .await;
    let rows = deliveries(&database, hook, 4).await;
    assert_eq!(rows[0].event, "ping");
    assert_eq!(rows[0].status_code, Some(200));
    let pings = receiver
        .received()
        .into_iter()
        .filter(|request| request.header("x-github-event") == Some("ping"))
        .count();
    assert_eq!(pings, 4, "the first ping, then the ping and two retries");

    // A missing confirmation keeps the hook; a confirmed delete removes it and its history.
    let (status, _) = alice
        .form(
            "/alice/proj/_forms/webhook-delete",
            &[("repository_id", &repo), ("hook_id", &hook.to_string())],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/webhook-delete",
            &[
                ("repository_id", &repo),
                ("hook_id", &hook.to_string()),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/alice/proj/settings?tab=webhooks&notice=deleted")
    );
    assert!(db::webhooks::list_webhooks(
        database.pool(),
        HookOwner::Repository(
            db::repositories::records::get_repository(database.pool(), "alice", "proj")
                .await
                .unwrap()
                .unwrap()
                .id
        )
    )
    .await
    .unwrap()
    .is_empty());
}

/// A conversation comment sends `issue_comment` when it is written. An inline comment in a pending
/// review is held back, and sends `pull_request_review_comment` when the review is submitted.
async fn pull_request_comment_hooks(pg: bool, url: &str) {
    let (database, _root, alice, _bob) = setup(pg).await;
    let repo = repo_id(&database).await;
    let reply = alice
        .form_reply(
            "/alice/proj/_forms/webhook-save",
            &save_pairs(
                &repo,
                url,
                &["issue_comment", "pull_request_review_comment"],
            ),
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    let hook = hook_id(&database).await;
    deliveries(&database, hook, 1).await;

    let base = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &base).await;
    let head = alice.save("topic", "topic.txt", &base).await;
    let pull = alice.open_pull("Review me", "topic", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let comments = format!("/api/repos/alice/proj/pulls/{number}/comments");

    alice
        .expect(
            "POST",
            &comments,
            json!({"body": "Conversation"}),
            StatusCode::CREATED,
        )
        .await;
    let rows = deliveries(&database, hook, 2).await;
    assert_eq!(rows[0].event, "issue_comment");
    assert_eq!(rows[0].action.as_deref(), Some("created"));
    // The issue is marked as a pull request, as GitHub receivers expect.
    let sent: Value = serde_json::from_str(&rows[0].request_body).unwrap();
    assert!(
        sent["issue"]["pull_request"]["html_url"].is_string(),
        "{sent}"
    );

    alice
        .expect(
            "POST",
            &comments,
            json!({"body": "On the line", "path": "topic.txt", "line": 1, "side": "RIGHT", "commit_sha": head, "pending": true}),
            StatusCode::CREATED,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        db::webhooks::list_deliveries(database.pool(), hook)
            .await
            .unwrap()
            .len(),
        2,
        "a pending inline comment is not sent"
    );
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/reviews"),
            json!({"state": "commented", "body": "Done", "commit_sha": head}),
            StatusCode::CREATED,
        )
        .await;
    let rows = deliveries(&database, hook, 3).await;
    assert_eq!(rows[0].event, "pull_request_review_comment");
    assert_eq!(rows[0].action.as_deref(), Some("created"));
}

/// One test runs both engines, because the receiver needs `WEBHOOKS_ALLOW_PRIVATE` for the
/// whole process and the refusal checks need it unset.
#[tokio::test]
async fn webhooks_on_sqlite_and_postgres() {
    refusals(false).await;
    refusals(true).await;
    std::env::set_var("WEBHOOKS_ALLOW_PRIVATE", "true");
    std::env::set_var("SECRETS_MASTER_KEY", "2a".repeat(32));
    for pg in [false, true] {
        let receiver = Receiver::default();
        let url = receiver.start().await;
        flow(pg, &receiver, &url).await;
        pull_request_comment_hooks(pg, &url).await;
    }
}

/// Each hook write checks, in its own transaction, that the actor may manage the owner's hooks.
/// So a demoted owner, or a user with no right to the repository, is refused by the write itself.
async fn hook_writes_check_authority_in_the_transaction(pg: bool) {
    let mut database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = database.actor("hook_owner").await;
    let second = database.actor("hook_second").await;
    let outsider = database.actor("hook_outsider").await;
    let pool = database.pool();
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let OrganizationOutcome::Applied(org) = orgs::create_organization(
        pool,
        owner,
        &orgs::NewOrganization::new(
            "hook_org",
            orgs::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap() else {
        panic!("the organization was not created");
    };
    let OrganizationOutcome::Applied(_) =
        orgs::add_organization_member(pool, owner, org.id, second, OrgRole::Owner)
            .await
            .unwrap()
    else {
        panic!("the second owner was not added");
    };
    // The first owner is demoted before any write of its own runs.
    let OrganizationOutcome::Applied(_) =
        orgs::update_organization_member_role(pool, second, org.id, owner, OrgRole::Member)
            .await
            .unwrap()
    else {
        panic!("the first owner was not demoted");
    };
    let fields = WebhookFields {
        payload_url: "https://example.com/hooks".to_owned(),
        content_type: "application/json".to_owned(),
        events: "push".to_owned(),
        active: true,
        ssl_verify: true,
    };
    let organization = HookOwner::Organization(org.id);
    assert_eq!(
        db::webhooks::insert_webhook(pool, organization, owner, &fields, None)
            .await
            .unwrap(),
        None,
        "a demoted owner created an organization hook"
    );
    let hook = db::webhooks::insert_webhook(pool, organization, second, &fields, None)
        .await
        .unwrap()
        .expect("a current owner creates a hook");
    assert_eq!(
        db::webhooks::update_webhook(pool, organization, owner, hook, &fields, None)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db::webhooks::delete_webhook(pool, organization, owner, hook)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db::webhooks::list_webhooks(pool, organization)
            .await
            .unwrap()
            .len(),
        1
    );

    let repository = HookOwner::Repository(repo.id);
    assert_eq!(
        db::webhooks::insert_webhook(pool, repository, outsider, &fields, None)
            .await
            .unwrap(),
        None,
        "a user without access created a repository hook"
    );
    let repository_hook = db::webhooks::insert_webhook(pool, repository, owner, &fields, None)
        .await
        .unwrap()
        .expect("the repository owner creates a hook");
    assert_eq!(
        db::webhooks::update_webhook(pool, repository, outsider, repository_hook, &fields, None)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db::webhooks::delete_webhook(pool, repository, outsider, repository_hook)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db::webhooks::list_webhooks(pool, repository)
            .await
            .unwrap()
            .len(),
        1
    );
    database.finish().await;
}

#[tokio::test]
async fn hook_writes_check_authority_on_sqlite_and_postgres() {
    hook_writes_check_authority_in_the_transaction(false).await;
    hook_writes_check_authority_in_the_transaction(true).await;
}
