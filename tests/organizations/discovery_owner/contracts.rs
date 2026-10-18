use super::fixture::{Fixture, COLLISION, ORG, USER};
use codeyard::{
    backend::db,
    webui::pages::discovery::{self as view, RepositoryCard},
};

const NAMES: [&str; 4] = [
    "discovery_personal",
    "discovery_organization",
    "discovery_personal_repeat",
    "discovery_organization_repeat",
];

fn public_cards(cards: &[RepositoryCard], order: &[usize]) {
    assert_eq!(cards.len(), order.len());
    for (card, index) in cards.iter().zip(order) {
        let namespace = if index % 2 == 0 { USER } else { ORG };
        assert_eq!(card.name, NAMES[*index]);
        assert_eq!(card.owner, namespace);
        assert!(!card.is_private);
        assert_eq!(
            view::repository_url(&card.owner, &card.name),
            format!("/{namespace}/{}", NAMES[*index]),
        );
    }
}

pub async fn run(f: &Fixture) {
    let pool = f.database.pool();
    for (index, id) in f.public.iter().enumerate() {
        let repo = db::repositories::records::get_repository_by_id(pool, *id)
            .await
            .unwrap()
            .unwrap();
        if index % 2 == 0 {
            assert_eq!(repo.owner_user_id, Some(COLLISION));
            assert_eq!(repo.owner_org_id, None);
        } else {
            assert_eq!(repo.owner_user_id, None);
            assert_eq!(repo.owner_org_id, Some(COLLISION));
        }
        assert_eq!(
            db::repositories::records::repository_owner_name(pool, *id)
                .await
                .unwrap()
                .as_deref(),
            Some(if index % 2 == 0 { USER } else { ORG }),
        );
    }
    // The equal-numbered user is not an organization member; neither membership
    // nor private repository metadata authority is conferred by a namespace read.
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, COLLISION, f.private_org)
            .await
            .unwrap(),
        db::models::AccessLevel::None,
    );
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, f.private_org)
            .await
            .unwrap()
            .as_deref(),
        Some(ORG),
    );
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, i64::MAX)
            .await
            .unwrap(),
        None
    );

    for order in [[0, 2, 1, 3], [1, 3, 0, 2]] {
        for (rank, index) in order.iter().enumerate() {
            let score = 40 - rank as i64;
            let timestamp = format!("2020-01-01T00:00:{score:02}.000000Z");
            f.database.execute(
                &format!("UPDATE repositories SET created_at='{timestamp}',updated_at='{timestamp}',stars_count=$1 WHERE id=$2"),
                &[score, f.public[*index]],
            )
            .await;
            f.database
                .execute(
                    &format!("UPDATE issues SET updated_at='{timestamp}' WHERE repo_id=$1"),
                    &[f.public[*index]],
                )
                .await;
        }
        // Private orgs have higher stars/newer dates and outnumber every bound.
        assert_eq!(
            db::repositories::records::list_public_repositories(
                pool,
                db::repositories::records::RepositorySort::Recent,
                1,
            )
            .await
            .unwrap()[0]
                .id,
            f.public[order[0]],
        );
        assert_eq!(
            db::repositories::discovery::search_repositories(
                pool,
                None,
                "discovery",
                db::repositories::records::RepositorySort::Stars,
                0,
                1
            )
            .await
            .unwrap()[0]
                .id,
            f.public[order[0]],
        );
        assert_eq!(
            db::repositories::discovery::search_issues(
                pool,
                None,
                "discovery",
                db::repositories::discovery::ItemSort::Updated,
                0,
                1
            )
            .await
            .unwrap()[0]
                .repo_id,
            f.public[order[0]],
        );
        for authenticated in [false, true] {
            for path in ["/explore", "/explore?sort=stars"] {
                let view::Page::Explore(model) = f.page(path, authenticated).await.discovery()
                else {
                    panic!("expected explore");
                };
                assert_eq!(model.repositories.len(), 4);
                assert!(!model.has_older);
                public_cards(&model.repositories, &order);
            }
            // The four repositories fit on page one, so page two is empty with nothing older.
            let view::Page::Explore(model) =
                f.page("/explore?page=2", authenticated).await.discovery()
            else {
                panic!("expected a later explore page");
            };
            assert!(model.repositories.is_empty());
            assert!(!model.has_older);
            for tab in ["repos", "issues", "users"] {
                let path = format!("/search?q=discovery&type={tab}");
                let view::Page::Search(model) = f.page(&path, authenticated).await.discovery()
                else {
                    panic!("expected search");
                };
                assert_eq!(model.tab, tab);
                assert_eq!(model.query, "discovery");
                // A search loads only the open tab's results, and each tab's badge is the total.
                match tab {
                    "repos" => {
                        if authenticated {
                            // The viewer reads their own private repository too, and its zero
                            // stars rank it after the public ones.
                            assert_eq!(model.repositories.len(), order.len() + 1);
                            public_cards(&model.repositories[..order.len()], &order);
                            let own = &model.repositories[order.len()];
                            assert_eq!(
                                (own.owner.as_str(), own.name.as_str()),
                                (USER, "personal_private")
                            );
                            assert!(own.is_private);
                        } else {
                            public_cards(&model.repositories, &order);
                        }
                        assert_eq!(model.counts.repositories, model.repositories.len());
                    }
                    "issues" => {
                        assert_eq!(model.issues.len(), 4);
                        for (issue, index) in model.issues.iter().zip(order) {
                            assert_eq!(issue.repo, NAMES[index]);
                            assert_eq!(issue.owner, if index % 2 == 0 { USER } else { ORG });
                            assert_eq!(issue.number, 1);
                        }
                        assert_eq!(model.counts.issues, 4);
                    }
                    _ => {
                        let user = model
                            .users
                            .iter()
                            .find(|user| user.username == USER)
                            .unwrap();
                        assert_eq!(
                            user.display_name.as_deref(),
                            Some("User display is not the namespace")
                        );
                        assert!(model.counts.users >= model.users.len());
                    }
                }
            }
        }
        // Personal list primes the same cache reused by public trending. The org
        // must retain its own slug even when the personal namespace was first.
        let view::Page::Home(model) = f.page("/", true).await.discovery() else {
            panic!("expected dashboard");
        };
        assert_eq!(model.username.as_deref(), Some(USER));
        assert_eq!(model.repositories.len(), 3);
        assert!(model.repositories.iter().all(|card| card.owner == USER));
        assert!(model
            .repositories
            .iter()
            .any(|card| card.name == "personal_private" && card.is_private));
        // Trending leaves out the viewer's own repositories (even public ones), so only the
        // organization's public repositories remain, in the same order.
        let others = order
            .iter()
            .copied()
            .filter(|index| index % 2 == 1)
            .collect::<Vec<_>>();
        public_cards(&model.trending, &others);
    }
    let view::Page::Home(model) = f.page("/", false).await.discovery() else {
        panic!("expected anonymous home");
    };
    assert!(model.username.is_none());
    assert!(model.repositories.is_empty());
    assert!(model.trending.is_empty());
    // Preserve search's existing name/description scope, not an owner/profile search.
    let view::Page::Search(model) = f.page("/search?q=discovery_user", false).await.discovery()
    else {
        panic!("expected search");
    };
    // The owner's name finds the owner's repositories, but never private ones.
    assert!(model
        .repositories
        .iter()
        .all(|card| card.owner == USER && !card.is_private));
    assert_eq!(model.counts.issues, 0);
    assert_eq!(model.counts.users, 1);
    let view::Page::Search(model) = f
        .page("/search?q=discovery_user&type=users", false)
        .await
        .discovery()
    else {
        panic!("expected people search");
    };
    assert_eq!(model.users.len(), 1);
    assert_eq!(model.users[0].username, USER);

    // Current identity names, not display_name, are projected on a new request.
    f.database
        .execute(
            "UPDATE users SET username='renamed_personal' WHERE id=700000",
            &[],
        )
        .await;
    f.database
        .execute(
            "UPDATE organizations SET name='renamed_organization' WHERE id=700000",
            &[],
        )
        .await;
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, f.public[0])
            .await
            .unwrap()
            .as_deref(),
        Some("renamed_personal")
    );
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, f.public[1])
            .await
            .unwrap()
            .as_deref(),
        Some("renamed_organization")
    );
    let view::Page::Explore(model) = f.page("/explore", false).await.discovery() else {
        panic!("expected renamed namespaces");
    };
    for card in model.repositories {
        let namespace = if card.name.contains("personal") {
            "renamed_personal"
        } else {
            "renamed_organization"
        };
        assert_eq!(card.owner, namespace);
    }
    f.database
        .execute(
            "UPDATE users SET username='discovery_user' WHERE id=700000",
            &[],
        )
        .await;
    f.database
        .execute(
            "UPDATE organizations SET name='discovery_org' WHERE id=700000",
            &[],
        )
        .await;
    // A deleted repository has no owner name to project.
    let deleted = f
        .database
        .insert_repository_row("org", "discovery_deleted", false)
        .await;
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, deleted)
            .await
            .unwrap()
            .as_deref(),
        Some(ORG)
    );
    f.database
        .execute("DELETE FROM repositories WHERE id=$1", &[deleted])
        .await;
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, deleted)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db::repositories::records::repository_owner_name(pool, f.own_private)
            .await
            .unwrap()
            .as_deref(),
        Some(USER)
    );
    discovery_api_contracts(f).await;
    api_contracts(f).await;
}

fn api_public_repositories(f: &Fixture, repositories: &serde_json::Value) {
    let repositories = repositories.as_array().unwrap();
    assert_eq!(repositories.len(), f.public.len());
    for (index, id) in f.public.iter().enumerate() {
        let repository = repositories.iter().find(|row| row["id"] == *id).unwrap();
        let owner = if index % 2 == 0 { USER } else { ORG };
        assert_eq!(repository["owner_id"], COLLISION);
        assert_eq!(repository["owner"], owner);
        assert_eq!(repository["name"], NAMES[index]);
        assert_eq!(repository["full_name"], format!("{owner}/{}", NAMES[index]));
        assert_eq!(repository["is_private"], false);
    }
}

fn api_public_issues(f: &Fixture, response: &serde_json::Value) {
    let issues = response["issues"].as_array().unwrap();
    assert_eq!(issues.len(), f.public.len());
    assert_eq!(response["issues_count"], issues.len());
    for (index, id) in f.public.iter().enumerate() {
        let issue = issues.iter().find(|row| row["repo_id"] == *id).unwrap();
        assert_eq!(issue["repo_name"], NAMES[index]);
        assert_eq!(issue["repo_owner"], if index % 2 == 0 { USER } else { ORG });
        assert_eq!(issue["number"], 1);
        assert_eq!(issue["author"]["username"], "discovery_governor");
        assert_ne!(issue["author"]["id"], COLLISION);
    }
}

async fn discovery_api_contracts(f: &Fixture) {
    use axum::http::StatusCode;

    for path in ["/api/explore", "/api/explore?sort=stars"] {
        let (status, response) = f.api(path).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        api_public_repositories(f, &response);
    }
    for search_type in ["", "&type=repos", "&type=issues"] {
        let (status, response) = f
            .api(&format!("/api/search?q=discovery{search_type}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        if search_type != "&type=issues" {
            api_public_repositories(f, &response["repos"]);
            assert_eq!(response["repos_count"], f.public.len());
        } else {
            assert!(response["repos"].as_array().unwrap().is_empty());
            assert_eq!(response["repos_count"], 0);
        }
        if search_type != "&type=repos" {
            api_public_issues(f, &response);
        } else {
            assert!(response["issues"].as_array().unwrap().is_empty());
            assert_eq!(response["issues_count"], 0);
        }
        if !search_type.is_empty() {
            assert!(response["users"].as_array().unwrap().is_empty());
            assert_eq!(response["users_count"], 0);
        }
    }
    let (status, response) = f.api("/api/search?q=discovery_user&type=users").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["users_count"], 1);
    assert_eq!(response["users"][0]["username"], USER);
    assert_eq!(response["repos_count"], 0);
    assert_eq!(response["issues_count"], 0);

    // The matching issues' repositories are older than the newest 1,000 rows.
    // The same rows exercise explore's default, minimum and maximum list size.
    f.database
        .execute(
            "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<1000) INSERT INTO repositories(owner_kind,owner_org_id,name,is_private,disk_path) SELECT 'org',700000,'api_recent_listing_' || n,FALSE,'/fixture/not-executed.git' FROM numbers",
            &[],
        )
        .await;
    for (path, count) in [
        ("/api/explore", 50),
        ("/api/explore?limit=0", 1),
        ("/api/explore?limit=1", 1),
        ("/api/explore?limit=100", 100),
        ("/api/explore?limit=101", 100),
        ("/api/explore?limit=4294967295", 100),
    ] {
        let (status, response) = f.api(path).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let repositories = response.as_array().unwrap();
        assert_eq!(repositories.len(), count, "{path}");
        assert!(repositories.iter().all(|repo| repo["owner"] == ORG));
    }
    for path in [
        "/api/search?q=discovery",
        "/api/search?q=discovery&type=issues",
    ] {
        let (status, response) = f.api(path).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        api_public_issues(f, &response);
    }
    f.database
        .execute(
            "DELETE FROM repositories WHERE owner_kind='org' AND owner_org_id=700000 AND name LIKE 'api_recent_listing_%'",
            &[],
        )
        .await;
    discovery_api_database_errors(f).await;
}

async fn discovery_api_database_errors(f: &Fixture) {
    use axum::http::StatusCode;

    // Renames are confined to the disposable fixture and restored for later contracts.
    for (table, search_type) in [
        ("repositories", "repos"),
        ("issues", "issues"),
        ("users", "users"),
        ("organizations", "repos"),
    ] {
        f.database
            .execute(
                &format!("ALTER TABLE {table} RENAME TO unavailable_{table}"),
                &[],
            )
            .await;
        let mut paths = vec![
            "/api/search?q=discovery".to_string(),
            format!("/api/search?q=discovery&type={search_type}"),
        ];
        if table == "repositories" || table == "organizations" {
            paths.push("/api/explore".to_string());
            paths.push("/api/search?q=discovery&type=issues".to_string());
        }
        for path in paths {
            let (status, error) = f.api(&path).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}: {error}");
            assert_eq!(error["error"], "database_error");
        }
        if table == "repositories" {
            // User-only search must not load unrelated repository lists.
            let (status, response) = f.api("/api/search?q=discovery_user&type=users").await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["users_count"], 1);
        }
        f.database
            .execute(
                &format!("ALTER TABLE unavailable_{table} RENAME TO {table}"),
                &[],
            )
            .await;
    }
}

async fn api_contracts(f: &Fixture) {
    use axum::http::StatusCode;
    let (status, list) = f.api("/api/repos").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let repositories = list.as_array().unwrap();
    assert_eq!(repositories.len(), 4);
    for (index, id) in f.public.iter().enumerate() {
        let repository = repositories.iter().find(|row| row["id"] == *id).unwrap();
        let owner = if index % 2 == 0 { USER } else { ORG };
        assert_eq!(repository["owner"], owner);
        assert_eq!(repository["full_name"], format!("{owner}/{}", NAMES[index]));
        let (status, detail) = f.api(&format!("/api/repos/{owner}/{}", NAMES[index])).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["owner"], owner);
    }
    // Imported forks are read projections, not an organization fork/copy permit.
    let public_fork = f
        .database
        .insert_repository_row("org", "api_org_fork", false)
        .await;
    let private_fork = f
        .database
        .insert_repository_row("org", "api_private_fork", true)
        .await;
    for fork in [public_fork, private_fork] {
        f.database
            .execute(
                "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2",
                &[f.public[1], fork],
            )
            .await;
    }
    let (status, forks) = f
        .api(&format!("/api/repos/{ORG}/discovery_organization/forks"))
        .await;
    assert_eq!(status, StatusCode::OK, "{forks}");
    let forks = forks.as_array().unwrap();
    assert_eq!(
        forks.len(),
        1,
        "private organization fork must not be listed"
    );
    assert_eq!(forks[0]["id"], public_fork);
    assert_eq!(forks[0]["owner"], ORG);
    assert_eq!(forks[0]["forked_from"]["owner"], ORG);
    let (status, detail) = f.api(&format!("/api/repos/{ORG}/api_org_fork")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["forked_from"]["owner"], ORG);
    // A private parent is hidden, not projected through its public fork.
    f.database
        .execute(
            "UPDATE repositories SET forked_from_id=$1 WHERE id=$2",
            &[f.private_org, public_fork],
        )
        .await;
    // Native ancestry visibility correctly makes this fork private too. Give
    // only the owned fixture session explicit Read on the child, not the parent.
    let (status, _) = f.api(&format!("/api/repos/{ORG}/api_org_fork")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    db::repositories::access::grant_repo_access(
        f.database.pool(),
        COLLISION,
        public_fork,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let (status, detail) = f
        .api_as(&format!("/api/repos/{ORG}/api_org_fork"), true)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["forked_from"].is_null());
    // A GET must not provision missing storage or force-publish a peer bundle.
    let missing = f.missing_git_path();
    f.database.set_disk_path(f.public[0], &missing).await;
    let (status, error) = f
        .api(&format!("/api/repos/{USER}/discovery_personal/branches"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert!(
        !missing.exists(),
        "read initialized a repository without storage admission"
    );
    assert!(!error.to_string().contains(missing.to_str().unwrap()));
}

pub async fn bounded_queries(f: &Fixture) {
    use super::fixture::count_queries;
    use axum::http::StatusCode;

    let base = f
        .database
        .insert_repository_row("user", "paged_base", false)
        .await;
    let mut fork_ids = Vec::new();
    for n in 0..125 {
        let kind = if n % 2 == 0 { "user" } else { "org" };
        let id = f
            .database
            .insert_repository_row(kind, &format!("paged_fork_{n}"), false)
            .await;
        f.database.execute("UPDATE repositories SET forked_from_id=$1,is_fork=TRUE,created_at='2024-01-01T00:00:00.000000Z' WHERE id=$2", &[base, id]).await;
        fork_ids.push(id);
    }
    // Creation time wins over the unique descending-ID tie breaker.
    f.database
        .execute(
            "UPDATE repositories SET created_at='2025-01-01T00:00:00.000000Z' WHERE id=$1",
            &[fork_ids[0]],
        )
        .await;
    let mut ordered_forks = fork_ids[1..].iter().rev().copied().collect::<Vec<_>>();
    ordered_forks.insert(0, fork_ids[0]);
    f.database.execute("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<125) INSERT INTO users(id,username,email) SELECT 710000+n,'paged_star_' || n,'private-star-' || n || '@example.test' FROM numbers", &[]).await;
    f.database.execute("INSERT INTO stars(repo_id,user_id,created_at) SELECT $1,id,'2024-01-01T00:00:00.000000Z' FROM users WHERE id BETWEEN 710001 AND 710125", &[base]).await;
    f.database.execute("UPDATE stars SET created_at='2025-01-01T00:00:00.000000Z' WHERE repo_id=$1 AND user_id=710001", &[base]).await;
    let mut ordered_stars = (710002..=710125).rev().collect::<Vec<i64>>();
    ordered_stars.insert(0, 710001);

    for (endpoint, expected) in [("forks", &ordered_forks), ("stargazers", &ordered_stars)] {
        let path = format!("/api/repos/{USER}/paged_base/{endpoint}");
        for (query, start, size) in [
            ("", 0, 30),
            ("?per_page=0", 0, 1),
            ("?per_page=1", 0, 1),
            ("?per_page=100", 0, 100),
            ("?per_page=4294967295", 0, 100),
            ("?per_page=100&page=2", 100, 25),
            ("?per_page=100&page=3", 125, 0),
        ] {
            let ((status, response), queries) =
                count_queries(f.api(&format!("{path}{query}"))).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let rows = response.as_array().unwrap();
            let ids = rows
                .iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(ids, expected[start..start + size], "{endpoint}{query}");
            assert!(
                queries <= 12,
                "{endpoint}: {queries} queries for {size} rows"
            );
            assert!(!response.to_string().contains("private-star-"));
            if endpoint == "forks" {
                for row in rows {
                    let index = fork_ids.iter().position(|id| row["id"] == *id).unwrap();
                    assert_eq!(row["owner"], if index % 2 == 0 { USER } else { ORG });
                    assert_eq!(row["forked_from"]["owner"], USER);
                }
            }
        }
        for page in [0, 1001] {
            assert_eq!(
                f.api(&format!("{path}?page={page}")).await.0,
                StatusCode::BAD_REQUEST
            );
        }
    }
    // Native callers cannot bypass the page cap either.
    assert_eq!(
        db::repositories::discovery::list_forks(f.database.pool(), base, i64::MAX, 0)
            .await
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        db::repositories::engagement::list_repo_stargazers(f.database.pool(), base, i64::MAX, 0)
            .await
            .unwrap()
            .len(),
        100
    );
    assert!(
        db::repositories::discovery::list_forks(f.database.pool(), base, 30, -1)
            .await
            .is_err()
    );
    assert!(
        db::repositories::engagement::list_repo_stargazers(f.database.pool(), base, 30, -1)
            .await
            .is_err()
    );

    // Private candidates consume page slots, not unbounded scanning or refilling.
    for n in 0..105 {
        let id = f
            .database
            .insert_repository_row("org", &format!("paged_hidden_{n}"), true)
            .await;
        f.database.execute("UPDATE repositories SET forked_from_id=$1,is_fork=TRUE,created_at='2026-01-01T00:00:00.000000Z' WHERE id=$2", &[base, id]).await;
    }
    let path = format!("/api/repos/{USER}/paged_base/forks");
    let ((status, rows), queries) = count_queries(f.api(&format!("{path}?per_page=100"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.as_array().unwrap().is_empty());
    assert!(queries <= 12, "anonymous private candidates: {queries}");
    let (status, rows) = f.api(&format!("{path}?per_page=100&page=2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.as_array().unwrap().len(), 95);
    assert_eq!(rows[0]["id"], ordered_forks[0]);

    let private = f
        .database
        .insert_repository_row("user", "paged_own_private", true)
        .await;
    f.database.execute("UPDATE repositories SET forked_from_id=$1,is_fork=TRUE,created_at='2027-01-01T00:00:00.000000Z' WHERE id=$2", &[base, private]).await;
    assert!(f
        .api(&format!("{path}?per_page=1"))
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        f.api_as(&format!("{path}?per_page=1"), true).await.1[0]["id"],
        private
    );
    let private_org = f
        .database
        .insert_repository_row("org", "paged_granted", true)
        .await;
    f.database.execute("UPDATE repositories SET forked_from_id=$1,is_fork=TRUE,created_at='2028-01-01T00:00:00.000000Z' WHERE id=$2", &[base, private_org]).await;
    assert!(f
        .api_as(&format!("{path}?per_page=1"), true)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    db::repositories::access::grant_repo_access(
        f.database.pool(),
        COLLISION,
        private_org,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    assert_eq!(
        f.api_as(&format!("{path}?per_page=1"), true).await.1[0]["id"],
        private_org
    );
    f.database
        .execute(
            "DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[private_org, COLLISION],
        )
        .await;
    assert!(f
        .api_as(&format!("{path}?per_page=1"), true)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    for endpoint in ["forks", "stargazers"] {
        assert_eq!(
            f.api(&format!("/api/repos/{USER}/paged_own_private/{endpoint}"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    let mut issue_repos = Vec::new();
    for n in 0..50 {
        let kind = if n % 2 == 0 { "user" } else { "org" };
        let id = f
            .database
            .insert_repository_row(kind, &format!("batch_repo_{n}"), false)
            .await;
        let author = if n % 2 == 0 { COLLISION } else { 710001 };
        f.database.execute("INSERT INTO issues(repo_id,number,author_id,title,body) VALUES($1,1,$2,'batch_issue','Fixture')", &[id, author]).await;
        issue_repos.push(id);
    }
    let ((status, rows), queries) = count_queries(f.api("/api/repos")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.as_array().unwrap().len(), 100);
    assert!(
        queries <= 10,
        "repository owner projection: {queries} queries"
    );
    let ((status, response), queries) =
        count_queries(f.api("/api/search?q=batch_issue&type=issues")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["issues_count"], 50);
    assert!(queries <= 12, "issue JSON projection: {queries} queries");
    for issue in response["issues"].as_array().unwrap() {
        let index = issue_repos
            .iter()
            .position(|id| issue["repo_id"] == *id)
            .unwrap();
        assert_eq!(issue["repo_owner"], if index % 2 == 0 { USER } else { ORG });
        assert_eq!(issue["repo_name"], format!("batch_repo_{index}"));
        assert_eq!(
            issue["author"]["username"],
            if index % 2 == 0 { USER } else { "paged_star_1" }
        );
        assert!(issue["author"].get("email").is_none());
    }
    // Issue projection adds three batched statements (repositories, owners and authors) whatever the
    // issue count, so compare with the same page when nothing matches.
    let (_, without_issues) =
        count_queries(f.page("/search?q=batch_issue_none&type=issues", false)).await;
    let (reply, queries) = count_queries(f.page("/search?q=batch_issue&type=issues", false)).await;
    let view::Page::Search(model) = reply.discovery() else {
        panic!("expected search")
    };
    // One page of issues: 30 per page, with the rest on later pages.
    assert_eq!(model.issues.len(), 30);
    assert!(
        queries <= without_issues + 3,
        "issue HTML/model projection: {queries} queries, {without_issues} without issues"
    );
    for issue in model.issues {
        let index = issue
            .repo
            .strip_prefix("batch_repo_")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(issue.owner, if index % 2 == 0 { USER } else { ORG });
    }
    f.database
        .execute(
            "UPDATE repositories SET is_private=TRUE WHERE id=$1",
            &[issue_repos[0]],
        )
        .await;
    let (status, response) = f.api("/api/search?q=batch_issue&type=issues").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["issues_count"], 49);
    assert!(response["issues"]
        .as_array()
        .unwrap()
        .iter()
        .all(|issue| issue["repo_id"] != issue_repos[0]));
}
