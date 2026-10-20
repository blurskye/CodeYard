//! Search over what each viewer can read, over HTTP on disposable SQLite and PostgreSQL data:
//! private, collaborator and organization repositories by visibility, code and commit search over
//! more repositories than a page holds, paging, and the note a budget leaves behind.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as http;

use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{
        self,
        models::{AccessLevel, Repository},
        organizations::{NewOrganization, OrganizationOutcome, OrganizationProfile},
        repositories::creation::{RepositoryCreationOwner, RepositoryTarget},
    },
    repositories::{
        lifecycle::provision,
        store::{repo_store, FileWrite, RepoStore},
    },
    storage::ObjectStore,
};
use database::Database;
use std::{path::Path, sync::Arc};

/// Commits `bytes` as a new file at `path` on `main`, after `tip`, and returns the new tip.
async fn save(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    store: &RepoStore,
    tip: &str,
    path: &str,
    bytes: &[u8],
) -> String {
    store
        .save_file_as(
            pool,
            repo_id,
            actor,
            &FileWrite {
                branch: "main".into(),
                path: path.into(),
                expected_revision: tip.into(),
                message: format!("Add {path}"),
                creating: true,
                deleting: false,
            },
            bytes,
            "Alice",
            "alice@example.invalid",
        )
        .await
        .unwrap()
        .sha
}

/// Creates a repository whose `main` holds `notes.txt` with `text`.
async fn repository(
    f: &Database,
    root: &Path,
    owner: RepositoryCreationOwner,
    name: &str,
    private: bool,
    text: &str,
) -> Repository {
    let repo = provision((f.pool(), None), root, owner, name, None, private, None)
        .await
        .unwrap();
    let store = repo_store(&repo, None);
    save(
        f.pool(),
        repo.id,
        owner.actor_id,
        &store,
        "unborn",
        "notes.txt",
        text.as_bytes(),
    )
    .await;
    repo
}

async fn router(f: &Database, root: &Path) -> axum::Router {
    create_router(AppState::new(
        f.pool().clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ))
}

/// The address of a code hit in `owner/name`'s `notes.txt` on `main`.
fn code_hit(owner: &str, name: &str) -> String {
    format!("/{owner}/{name}/blob/main/notes.txt#L1")
}

async fn visibility_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = f.actor("search_alice").await;
    let bob = f.actor("search_bob").await;
    let carol = f.actor("search_carol").await;
    let root = tempfile::tempdir().unwrap();

    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(alice),
        "open",
        false,
        "open-token here",
    )
    .await;
    let secret = repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(alice),
        "secret",
        true,
        "vault-token here",
    )
    .await;
    db::repositories::access::grant_repo_access(f.pool(), bob, secret.id, AccessLevel::Read)
        .await
        .unwrap();
    let acme = match db::organizations::create_organization(
        f.pool(),
        alice,
        &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
    )
    .await
    .unwrap()
    {
        OrganizationOutcome::Applied(metadata) => metadata,
        _ => panic!("the owner creates the organization"),
    };
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Organization(acme.id),
        },
        "internal",
        true,
        "org-token here",
    )
    .await;

    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);
    let bob_client = http::Client::login(&router, f.pool(), bob).await;
    let carol_client = http::Client::login(&router, f.pool(), carol).await;
    let alice_client = http::Client::login(&router, f.pool(), alice).await;

    // Everyone reads the public repository. Anonymous visitors and strangers read no private one.
    // The collaborator also reads the private repository shared with them. The organization owner
    // reads the organization's private repository and their own private repository.
    let cases = [
        ("anonymous", &anonymous, false, false, true),
        ("collaborator", &bob_client, true, false, true),
        ("stranger", &carol_client, false, false, true),
        ("owner", &alice_client, true, true, true),
    ];
    for (who, client, vault, org, open) in cases {
        for (query, path, visible) in [
            ("vault-token", code_hit("search_alice", "secret"), vault),
            ("org-token", code_hit("acme", "internal"), org),
            ("open-token", code_hit("search_alice", "open"), open),
        ] {
            let (status, html) = client.page(&format!("/search?q={query}&type=code")).await;
            assert_eq!(status, StatusCode::OK, "{who} {query}: {html}");
            assert_eq!(html.contains(&path), visible, "{who} sees {path}: {html}");
        }
        // The commit tab reads the same repositories.
        let (status, html) = client.page("/search?q=notes.txt&type=commits").await;
        assert_eq!(status, StatusCode::OK, "{who}: {html}");
        assert_eq!(
            html.contains("/search_alice/secret/commit/"),
            vault,
            "{who} commits"
        );
        assert_eq!(
            html.contains("/acme/internal/commit/"),
            org,
            "{who} org commits"
        );
        assert!(html.contains("/search_alice/open/commit/"), "{who}");
    }

    // The repository list follows the same rule, and so does the JSON search.
    let (status, html) = bob_client.page("/search?q=secret&type=repos").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("href=\"/search_alice/secret\""), "{html}");
    let (status, html) = anonymous.page("/search?q=secret&type=repos").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(!html.contains("href=\"/search_alice/secret\""), "{html}");
    let (status, body) = bob_client
        .api(
            "GET",
            "/api/search?q=secret&type=repos",
            serde_json::Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repos_count"], 1, "{body}");
    let (status, body) = anonymous
        .api(
            "GET",
            "/api/search?q=secret&type=repos",
            serde_json::Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repos_count"], 0, "{body}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_search_follows_repository_visibility() {
    visibility_contract(false).await;
}

#[tokio::test]
async fn postgres_search_follows_repository_visibility() {
    visibility_contract(true).await;
}

/// Thirty-five public repositories hold one matching line each. Every one of them is searched:
/// the first page holds thirty of the hits and the second the other five, and the links between
/// them follow the page order.
async fn paging_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_pager").await;
    let root = tempfile::tempdir().unwrap();
    for number in 0..35 {
        repository(
            &f,
            root.path(),
            RepositoryCreationOwner::personal(owner),
            &format!("pager-{number:02}"),
            false,
            "pager-token line",
        )
        .await;
    }
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, first) = anonymous.page("/search?q=pager-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = anonymous
        .page("/search?q=pager-token&type=code&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let hits = |html: &str| {
        (0..35)
            .filter(|number| {
                html.contains(&code_hit("search_pager", &format!("pager-{number:02}")))
            })
            .count()
    };
    assert_eq!(hits(&first), 30, "{first}");
    assert_eq!(hits(&second), 5, "{second}");
    assert!(first.contains(">Older<") && !first.contains(">Newer<"));
    assert!(second.contains(">Newer<") && !second.contains(">Older<"));
    assert!(!first.contains("Searched "), "no budget stopped the walk");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_code_search_pages_through_every_repository() {
    paging_contract(false).await;
}

#[tokio::test]
async fn postgres_code_search_pages_through_every_repository() {
    paging_contract(true).await;
}

/// A repository whose stored size exceeds the request's byte budget is searched alone; the walk
/// then stops and the page says how far it got. Narrowing with `repo:` searches the one named.
async fn budget_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_budget").await;
    let root = tempfile::tempdir().unwrap();
    for name in ["budget-a", "budget-b", "budget-c"] {
        repository(
            &f,
            root.path(),
            RepositoryCreationOwner::personal(owner),
            name,
            false,
            "budget-token line",
        )
        .await;
    }
    // Six hundred mebibytes: more than the whole byte budget of one request. It is also the most
    // recently updated repository, so the walk meets it first.
    f.execute(
        "UPDATE repositories SET size_bytes = 629145600, updated_at = '2099-01-01T00:00:00.000000Z' WHERE name = 'budget-a'",
        &[],
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, html) = anonymous.page("/search?q=budget-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&code_hit("search_budget", "budget-a")),
        "{html}"
    );
    assert!(
        !html.contains(&code_hit("search_budget", "budget-b")),
        "{html}"
    );
    assert!(
        html.contains("Searched 1 of 3 repositories, narrow with repo:/user:/org:"),
        "{html}"
    );

    let (status, html) = anonymous
        .page("/search?q=repo%3Asearch_budget%2Fbudget-b%20budget-token&type=code")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&code_hit("search_budget", "budget-b")),
        "{html}"
    );
    assert!(!html.contains("Searched "), "{html}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_code_search_reports_a_stopped_walk() {
    budget_contract(false).await;
}

#[tokio::test]
async fn postgres_code_search_reports_a_stopped_walk() {
    budget_contract(true).await;
}

/// The page without the comment markers that hydration leaves between text nodes, so a badge's
/// number reads as one piece of text.
fn visible(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Repositories a viewer can read, each as a card on the repositories tab.
fn repository_links(html: &str, owner: &str) -> Vec<String> {
    html.match_indices(&format!("href=\"/{owner}/bucket-"))
        .map(|(start, _)| {
            let rest = &html[start + "href=\"/".len() + owner.len() + 1..];
            rest[..rest.find('"').unwrap()].to_owned()
        })
        .collect()
}

/// Thirty-five public repositories match "bucket", and each has as many forks as its number.
/// The fork order puts 34 first, so the first page of forks holds 34 down to 05 and the second
/// page holds the rest. Sorting in the database keeps that order across the whole result set.
async fn buckets_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_bucket").await;
    let root = tempfile::tempdir().unwrap();
    let mut first_repo = None;
    for number in 0..35 {
        let repo = repository(
            &f,
            root.path(),
            RepositoryCreationOwner::personal(owner),
            &format!("bucket-{number:02}"),
            false,
            "bucket text",
        )
        .await;
        f.execute(
            &format!(
                "UPDATE repositories SET forks_count = {number} WHERE id = {}",
                repo.id
            ),
            &[],
        )
        .await;
        first_repo.get_or_insert(repo.id);
    }
    let repo_id = first_repo.unwrap();
    // A private repository the owner can read, and anonymous visitors cannot.
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "bucket-private",
        true,
        "bucket text",
    )
    .await;
    // Issues, pull requests and people, one more than a page of each.
    for number in 1..=35 {
        db::issues::create_issue(
            f.pool(),
            repo_id,
            owner,
            &format!("bucket issue {number:02}"),
            "",
        )
        .await
        .unwrap();
        db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                head_repo_id: None,
                repo_id,
                author_id: owner,
                title: &format!("bucket pull {number:02}"),
                body: "",
                head_branch: &format!("bucket-{number:02}"),
                head_sha: "1111111111111111111111111111111111111111",
                base_branch: "main",
                base_sha: "0000000000000000000000000000000000000000",
                is_draft: false,
            },
        )
        .await
        .unwrap();
        db::users::accounts::create_user(f.pool(), &format!("bucket_person_{number:02}"), None)
            .await
            .unwrap();
    }

    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);
    let owner_client = http::Client::login(&router, f.pool(), owner).await;

    // Repositories: the badge counts every match the viewer can read, and forks order the pages.
    let (status, first) = anonymous
        .page("/search?q=bucket&type=repos&sort=forks")
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert!(visible(&first).contains(">35</span>"), "{first}");
    let page_one = repository_links(&first, "search_bucket");
    assert_eq!(page_one.len(), 30, "{page_one:?}");
    assert_eq!(page_one.first().map(String::as_str), Some("bucket-34"));
    assert_eq!(page_one.last().map(String::as_str), Some("bucket-05"));
    assert!(first.contains(">Older<") && !first.contains(">Newer<"));
    let (status, second) = anonymous
        .page("/search?q=bucket&type=repos&sort=forks&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let page_two = repository_links(&second, "search_bucket");
    assert_eq!(page_two.len(), 5, "{page_two:?}");
    assert_eq!(page_two.first().map(String::as_str), Some("bucket-04"));
    assert!(second.contains(">Newer<") && !second.contains(">Older<"));
    // The owner also reads the private repository, so the total and the first page change.
    let (status, own) = owner_client.page("/search?q=bucket&type=repos").await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert!(visible(&own).contains(">36</span>"), "{own}");
    assert!(
        own.contains("href=\"/search_bucket/bucket-private\""),
        "{own}"
    );
    assert!(!first.contains("href=\"/search_bucket/bucket-private\""));

    // Issues: the default order is the most recently updated, and `oldest` reverses it, each
    // across both pages.
    let (status, newest) = anonymous.page("/search?q=bucket&type=issues").await;
    assert_eq!(status, StatusCode::OK, "{newest}");
    assert!(newest.contains("bucket issue 35") && newest.contains("bucket issue 06"));
    assert!(!newest.contains("bucket issue 05"), "{newest}");
    let (status, oldest) = anonymous
        .page("/search?q=bucket&type=issues&sort=oldest")
        .await;
    assert_eq!(status, StatusCode::OK, "{oldest}");
    assert!(oldest.contains("bucket issue 01") && oldest.contains("bucket issue 30"));
    assert!(!oldest.contains("bucket issue 31"), "{oldest}");
    assert!(visible(&oldest).contains(">35</span>"), "{oldest}");
    let (status, rest) = anonymous
        .page("/search?q=bucket&type=issues&sort=oldest&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert!(rest.contains("bucket issue 31") && rest.contains("bucket issue 35"));
    assert!(!rest.contains("bucket issue 30"), "{rest}");
    assert!(!rest.contains(">Older<") && rest.contains(">Newer<"));

    // Pull requests page the same way.
    let (status, pulls) = anonymous
        .page("/search?q=bucket&type=pulls&sort=oldest")
        .await;
    assert_eq!(status, StatusCode::OK, "{pulls}");
    assert!(pulls.contains("bucket pull 01") && pulls.contains("bucket pull 30"));
    assert!(!pulls.contains("bucket pull 31"), "{pulls}");
    assert!(pulls.contains(">Older<"), "{pulls}");
    let (status, pulls) = anonymous
        .page("/search?q=bucket&type=pulls&sort=oldest&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{pulls}");
    assert!(pulls.contains("bucket pull 31") && pulls.contains("bucket pull 35"));

    // People: the names are ordered, and the badge counts all of them.
    let (status, people) = anonymous.page("/search?q=bucket_person&type=users").await;
    assert_eq!(status, StatusCode::OK, "{people}");
    assert!(visible(&people).contains(">35</span>"), "{people}");
    assert!(people.contains("href=\"/bucket_person_30\""));
    assert!(!people.contains("href=\"/bucket_person_31\""), "{people}");
    let (status, people) = anonymous
        .page("/search?q=bucket_person&type=users&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{people}");
    assert!(
        people.contains("href=\"/bucket_person_31\"")
            && people.contains("href=\"/bucket_person_35\"")
    );
    assert!(!people.contains("href=\"/bucket_person_30\""), "{people}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_search_buckets_page_and_count_every_match() {
    buckets_contract(false).await;
}

#[tokio::test]
async fn postgres_search_buckets_page_and_count_every_match() {
    buckets_contract(true).await;
}

/// A repository whose files cannot be read is reported on the page, and the repositories that
/// can be read still show their hits.
async fn unreadable_repository_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_unreadable").await;
    let root = tempfile::tempdir().unwrap();
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "walk-ok",
        false,
        "walk-token line",
    )
    .await;
    let broken = repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "walk-broken",
        false,
        "walk-token line",
    )
    .await;
    // The repository's directory disappears, as an unreadable storage would make it.
    std::fs::remove_dir_all(repo_store(&broken, None).location()).unwrap();

    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);
    let (status, html) = anonymous.page("/search?q=walk-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&code_hit("search_unreadable", "walk-ok")),
        "{html}"
    );
    assert!(
        !html.contains(&code_hit("search_unreadable", "walk-broken")),
        "{html}"
    );
    assert!(
        visible(&html).contains("1 repository could not be searched."),
        "{html}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_unreadable_repository_is_reported_and_the_rest_show() {
    unreadable_repository_contract(false).await;
}

#[tokio::test]
async fn postgres_unreadable_repository_is_reported_and_the_rest_show() {
    unreadable_repository_contract(true).await;
}

/// A repository with no commits has nothing to find: it is not a failure, and nothing is reported.
async fn empty_repository_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_empty").await;
    let root = tempfile::tempdir().unwrap();
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "walk-full",
        false,
        "walk-token line",
    )
    .await;
    provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "walk-empty",
        None,
        false,
        None,
    )
    .await
    .unwrap();

    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);
    for path in [
        "/search?q=walk-token&type=code",
        "/search?q=walk-token&type=commits",
    ] {
        let (status, html) = anonymous.page(path).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(!html.contains("could not be searched"), "{path}: {html}");
    }
    let (_, html) = anonymous.page("/search?q=walk-token&type=code").await;
    assert!(
        html.contains(&code_hit("search_empty", "walk-full")),
        "{html}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_empty_repository_is_not_reported_as_unreadable() {
    empty_repository_contract(false).await;
}

#[tokio::test]
async fn postgres_empty_repository_is_not_reported_as_unreadable() {
    empty_repository_contract(true).await;
}

/// The walk meets the most recently updated repository first, so a byte budget cuts the stalest.
async fn budget_cuts_the_least_recent_repository_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_recent").await;
    let root = tempfile::tempdir().unwrap();
    for name in ["recent-old", "recent-mid", "recent-new"] {
        repository(
            &f,
            root.path(),
            RepositoryCreationOwner::personal(owner),
            name,
            false,
            "recent-token line",
        )
        .await;
    }
    f.execute(
        "UPDATE repositories SET updated_at = '2000-01-01T00:00:00.000000Z', size_bytes = 629145600 WHERE name = 'recent-old'",
        &[],
    )
    .await;
    f.execute(
        "UPDATE repositories SET updated_at = '2010-01-01T00:00:00.000000Z' WHERE name = 'recent-mid'",
        &[],
    )
    .await;
    f.execute(
        "UPDATE repositories SET updated_at = '2099-01-01T00:00:00.000000Z' WHERE name = 'recent-new'",
        &[],
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);
    let (status, html) = anonymous.page("/search?q=recent-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&code_hit("search_recent", "recent-new")),
        "{html}"
    );
    assert!(
        html.contains(&code_hit("search_recent", "recent-mid")),
        "{html}"
    );
    assert!(
        !html.contains(&code_hit("search_recent", "recent-old")),
        "{html}"
    );
    assert!(
        html.contains("Searched 2 of 3 repositories, narrow with repo:/user:/org:"),
        "{html}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_budget_cuts_the_least_recently_updated_repository() {
    budget_cuts_the_least_recent_repository_contract(false).await;
}

#[tokio::test]
async fn postgres_budget_cuts_the_least_recently_updated_repository() {
    budget_cuts_the_least_recent_repository_contract(true).await;
}

/// Creates a repository whose `main` holds each of `files`, one commit per file.
async fn repository_with_files(
    f: &Database,
    root: &Path,
    owner: RepositoryCreationOwner,
    name: &str,
    files: &[(String, String)],
) -> Repository {
    let repo = provision((f.pool(), None), root, owner, name, None, false, None)
        .await
        .unwrap();
    let store = repo_store(&repo, None);
    let mut tip = "unborn".to_owned();
    for (path, text) in files {
        tip = save(
            f.pool(),
            repo.id,
            owner.actor_id,
            &store,
            &tip,
            path,
            text.as_bytes(),
        )
        .await;
    }
    repo
}

/// `repo:owner/name` searches that one repository. `repo:owner/app` does not also match `app2`.
async fn exact_repository_scope_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_scope").await;
    let root = tempfile::tempdir().unwrap();
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "app",
        false,
        "scope-token one",
    )
    .await;
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "app2",
        false,
        "scope-token two",
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, html) = anonymous
        .page("/search?q=scope-token%20repo:search_scope/app&type=code")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains(&code_hit("search_scope", "app")), "{html}");
    assert!(!html.contains("/search_scope/app2/"), "{html}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_repo_scope_names_one_repository() {
    exact_repository_scope_contract(false).await;
}

#[tokio::test]
async fn postgres_repo_scope_names_one_repository() {
    exact_repository_scope_contract(true).await;
}

/// A named repository pages through every hit. Each file has two matching lines, so sixteen files
/// hold thirty-two hits: thirty on the first page and two on the second.
async fn repository_scope_paging_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_pages").await;
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(String, String)> = (0..16)
        .map(|number| {
            (
                format!("file-{number:02}.txt"),
                "page-token one\npage-token two\n".to_owned(),
            )
        })
        .collect();
    repository_with_files(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "paged",
        &files,
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, first) = anonymous
        .page("/search?q=page-token%20repo:search_pages/paged&type=code")
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = anonymous
        .page("/search?q=page-token%20repo:search_pages/paged&type=code&page=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let hits = |html: &str| html.matches("/search_pages/paged/blob/main/").count();
    assert_eq!(hits(&first), 30, "{first}");
    assert_eq!(hits(&second), 2, "{second}");
    assert!(first.contains(">Older<") && !first.contains(">Newer<"));
    assert!(second.contains(">Newer<") && !second.contains(">Older<"));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_repo_scope_pages_through_every_hit() {
    repository_scope_paging_contract(false).await;
}

#[tokio::test]
async fn postgres_repo_scope_pages_through_every_hit() {
    repository_scope_paging_contract(true).await;
}

async fn one_file_and_unrelated_tables_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let contributions_before = db::users::accounts::list_admin_activity_points(f.pool(), 7)
        .await
        .unwrap()
        .last()
        .unwrap()
        .daily_contributions;
    let owner = f.actor("one_file").await;
    let root = tempfile::tempdir().unwrap();
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "many",
        false,
        &"line-token\n".repeat(40),
    )
    .await;
    // Web/store writes record commits without any contributor-cache population.
    let totals = db::users::accounts::list_admin_account_summaries(f.pool())
        .await
        .unwrap();
    assert_eq!(
        totals
            .iter()
            .find(|user| user.id == owner)
            .unwrap()
            .commits_total,
        1
    );
    let activity = db::users::accounts::list_admin_activity_points(f.pool(), 7)
        .await
        .unwrap();
    // The repository and its first commit both contribute.
    assert_eq!(
        activity.last().unwrap().daily_contributions,
        contributions_before + 2
    );
    // Explicit content search must not query discussion tables for unrelated tab counts.
    f.execute("ALTER TABLE issues RENAME TO hidden_issues", &[])
        .await;
    f.execute("ALTER TABLE pull_requests RENAME TO hidden_pulls", &[])
        .await;
    let router = router(&f, root.path()).await;
    let client = http::Client::anonymous(&router);
    for (page, count) in [(1, 30), (2, 10)] {
        let (status, html) = client
            .page(&format!(
                "/search?q=line-token%20repo:one_file/many&type=code&page={page}"
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("/one_file/many/blob/main/notes.txt#L").count(),
            count
        );
    }
    let (status, html) = client
        .page("/search?q=Add%20repo:one_file/many&type=commits")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Add notes.txt"));
    f.finish().await;
}
#[tokio::test]
async fn sqlite_one_file_pages_and_content_search_skips_other_counts() {
    one_file_and_unrelated_tables_contract(false).await;
}
#[tokio::test]
async fn postgres_one_file_pages_and_content_search_skips_other_counts() {
    one_file_and_unrelated_tables_contract(true).await;
}

/// A repository with more matches than a walk shows contributes five hits and a note that names it.
async fn truncation_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_trunc").await;
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(String, String)> = (0..3)
        .map(|number| {
            (
                format!("part-{number}.txt"),
                "trunc-token one\ntrunc-token two\n".to_owned(),
            )
        })
        .collect();
    repository_with_files(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "many",
        &files,
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, html) = anonymous.page("/search?q=trunc-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(
        html.matches("/search_trunc/many/blob/main/").count(),
        5,
        "{html}"
    );
    assert!(
        html.contains("More matches in search_trunc/many, narrow with repo:search_trunc/many"),
        "{html}"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_code_search_says_when_a_repository_has_more_matches() {
    truncation_contract(false).await;
}

#[tokio::test]
async fn postgres_code_search_says_when_a_repository_has_more_matches() {
    truncation_contract(true).await;
}

/// A matching line longer than git's old output limit still yields its hit, and the repository is
/// not reported as unsearched.
async fn long_match_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_wide").await;
    let root = tempfile::tempdir().unwrap();
    let long_line = format!("wide-token {}", "x".repeat(100 * 1024));
    repository(
        &f,
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "wide",
        false,
        &long_line,
    )
    .await;
    let router = router(&f, root.path()).await;
    let anonymous = http::Client::anonymous(&router);

    let (status, html) = anonymous.page("/search?q=wide-token&type=code").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains(&code_hit("search_wide", "wide")), "{html}");
    assert!(!html.contains("could not be searched"), "{html}");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_code_search_returns_hits_from_very_long_lines() {
    long_match_contract(false).await;
}

#[tokio::test]
async fn postgres_code_search_returns_hits_from_very_long_lines() {
    long_match_contract(true).await;
}

/// A count stops at the cap it is given, so a result set larger than the cap is counted as the cap.
async fn count_cap_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("search_cap").await;
    let root = tempfile::tempdir().unwrap();
    for number in 0..5 {
        repository(
            &f,
            root.path(),
            RepositoryCreationOwner::personal(owner),
            &format!("capped-{number}"),
            false,
            "cap text",
        )
        .await;
    }
    let pool = f.pool();
    assert_eq!(
        db::repositories::discovery::count_repositories(pool, None, "capped", 3)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        db::repositories::discovery::count_repositories(pool, None, "capped", 100)
            .await
            .unwrap(),
        5
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_search_counts_stop_at_their_cap() {
    count_cap_contract(false).await;
}

#[tokio::test]
async fn postgres_search_counts_stop_at_their_cap() {
    count_cap_contract(true).await;
}
