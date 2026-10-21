//! Bounded database-backed discussion pages and recoverable native submissions.
use super::templates::{hidden_value, put, setup};
use axum::http::StatusCode;
use codeyard::{
    backend::db,
    webui::{app::Page, pages::repos::issues},
};
use serde_json::Value;

async fn milestone_models(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let description = if pg {
        "repeat('x', 65536)"
    } else {
        "replace(hex(zeroblob(32768)), '0', 'x')"
    };
    database.execute(&format!("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1002) INSERT INTO milestones(repo_id, number, title, description) SELECT {}, x, 'M' || CAST(x AS TEXT), CASE WHEN x<=260 THEN {description} ELSE NULL END FROM n", repo.id), &[]).await;
    let choices = db::milestones::list_milestone_choices(database.pool(), repo.id)
        .await
        .unwrap();
    assert_eq!(choices.len(), 1000);
    assert!(choices.iter().all(|choice| choice.description.is_none()));
    let issue = db::issues::create_issue(
        database.pool(),
        repo.id,
        repo.owner_user_id.unwrap(),
        "History",
        "",
    )
    .await
    .unwrap();
    db::milestones::set_issue_milestone(
        database.pool(),
        repo.owner_user_id.unwrap(),
        repo.id,
        issue.id,
        Some(1002),
    )
    .await
    .unwrap();
    for path in [
        "/alice/proj/issues/new",
        "/alice/proj/issues/1",
        "/alice/proj/issues",
        "/alice/proj/pulls",
    ] {
        let (status, page) = alice.model(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            serde_json::to_vec(&page).unwrap().len() < 2 * 1024 * 1024,
            "{path}"
        );
        let Page::Collaboration(page) = page else {
            panic!("not collaboration")
        };
        match *page {
            issues::Page::New { choices, .. } => assert!(choices
                .milestones
                .iter()
                .all(|m| m.description.is_none() && m.open_items == 0)),
            issues::Page::Detail { milestones, .. } => assert!(milestones
                .iter()
                .all(|m| m.description.is_none() && m.open_items == 0)),
            issues::Page::List { items, filters, .. } => {
                assert_eq!(filters.milestones.len(), 1000);
                assert!(filters
                    .milestones
                    .iter()
                    .all(|m| m.description.is_none() && m.open_items == 0));
                if let Some(item) = items.first() {
                    assert_eq!(item.milestone.as_ref().unwrap().number, 1002);
                }
            }
            other => panic!("unexpected page: {other:?}"),
        }
    }
    let mut first_numbers = Vec::new();
    for number in [1, 2] {
        let (status, page) = alice
            .model(&format!("/alice/proj/milestones?page={number}"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(serde_json::to_vec(&page).unwrap().len() < 4 * 1024 * 1024);
        let Page::Collaboration(page) = page else {
            panic!("not collaboration")
        };
        let issues::Page::Milestones {
            items,
            counts,
            page_number,
            has_next,
            ..
        } = *page
        else {
            panic!("not milestones")
        };
        assert_eq!(counts.open, 1002);
        assert_eq!(page_number, number);
        assert!(has_next);
        assert_eq!(items.len(), 30);
        assert!(items
            .iter()
            .all(|m| m.description.as_ref().unwrap().len() == 65536));
        if number == 1 {
            first_numbers = items.iter().map(|m| m.number).collect();
        } else {
            assert!(items.iter().all(|m| !first_numbers.contains(&m.number)));
        }
    }
    let (status, html) = alice
        .page("/alice/proj/milestones?page=2&sort=alphabetical")
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        html.chars().take(500).collect::<String>()
    );
    let html = html.replace("&#38;", "&").replace("&amp;", "&");
    let links: Vec<_> = html
        .split("href=\"")
        .skip(1)
        .filter_map(|tail| tail.split('"').next())
        .filter(|href| href.contains("milestones?"))
        .collect();
    assert!(
        html.contains("sort=alphabetical&page=1"),
        "links: {links:?}"
    );
    assert!(
        html.contains("sort=alphabetical&page=3"),
        "links: {links:?}"
    );
    database.finish().await;
}

async fn templates_offline(pg: bool) {
    let (mut database, root, alice, bob) = setup(pg).await;
    let path = ".github/ISSUE_TEMPLATE/form.yml";
    put(
        &alice,
        "main",
        path,
        "name: Report\nbody:\n  - type: textarea\n    attributes:\n      label: Steps\n",
        "unborn",
    )
    .await;
    let (_, html) = bob
        .page("/alice/proj/issues/new?template=.github%2FISSUE_TEMPLATE%2Fform.yml")
        .await;
    let revision = hidden_value(&html, "template_revision");
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let id = repo.id.to_string();
    std::fs::rename(&repo.disk_path, root.path().join("offline.git")).unwrap();
    let (status, html) = bob.page("/alice/proj/issues/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Issue templates are unavailable"));
    let refused = bob
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("repository_id", &id),
                ("title", "Report"),
                ("template", path),
                ("template_revision", &revision),
                ("form_0", "Carefully written answers"),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text()
    );
    assert!(refused.text().contains("Carefully written answers"));
    assert!(refused.text().contains("Issue templates are unavailable"));
    assert!(!refused.text().contains("name=\"template_revision\""));
    let saved = bob
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("repository_id", &id),
                ("title", "Report"),
                ("body", "form_0: Carefully written answers"),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    for path in [
        "/alice/proj/issues",
        "/alice/proj/pulls",
        "/alice/proj/issues/1",
        "/alice/proj/milestones",
    ] {
        assert_eq!(bob.page(path).await.0, StatusCode::OK, "{path}");
    }
    database.finish().await;
}

async fn label_submissions(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = repo.owner_user_id.unwrap();
    let mut ids = Vec::new();
    for n in 0..101 {
        let name = match n {
            0 => "bug".into(),
            1 => " bug ".into(),
            _ => format!("label-{n}"),
        };
        ids.push(
            db::labels::create_label(database.pool(), actor, repo.id, &name, "123456", None)
                .await
                .unwrap()
                .id
                .to_string(),
        );
    }
    let repository_id = repo.id.to_string();
    let mut pairs = vec![
        ("repository_id", repository_id.as_str()),
        ("title", "Too many labels"),
        ("body", "Retained draft"),
        ("intent", "create"),
    ];
    pairs.extend(ids.iter().map(|id| ("labels", id.as_str())));
    let refused = alice.form_reply("/alice/proj/issues/new", &pairs).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.text().contains("Choose at most 100 labels"));
    assert!(
        db::issues::list_repo_issues(database.pool(), repo.id, &Default::default(), 100)
            .await
            .unwrap()
            .is_empty()
    );
    pairs.pop();
    assert_eq!(
        alice
            .form_reply("/alice/proj/issues/new", &pairs)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let labels = db::labels::get_issue_labels(
        database.pool(),
        db::issues::get_issue(database.pool(), repo.id, 1)
            .await
            .unwrap()
            .unwrap()
            .id,
    )
    .await
    .unwrap();
    assert_eq!(labels.len(), 100);
    // One exact label on each issue, so trimming would return the wrong row.
    for (index, label) in ids.iter().take(2).enumerate() {
        let issue = db::issues::create_issue(
            database.pool(),
            repo.id,
            actor,
            &format!("Exact {index}"),
            "",
        )
        .await
        .unwrap();
        db::labels::add_issue_labels(
            database.pool(),
            actor,
            repo.id,
            issue.id,
            &[label.parse().unwrap()],
        )
        .await
        .unwrap();
    }
    for query in ["label=%20bug%20", "q=label%3A%22%20bug%20%22"] {
        let (_, page) = alice.model(&format!("/alice/proj/issues?{query}")).await;
        let Page::Collaboration(page) = page else {
            panic!("not collaboration")
        };
        let issues::Page::List { items, .. } = *page else {
            panic!("not list")
        };
        assert_eq!(items.iter().map(|i| i.number).collect::<Vec<_>>(), [3, 1]);
    }
    let exact = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues?label=%20bug%20",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(exact.as_array().unwrap().len(), 2);
    // A failed sidebar write must redirect after creating, never leave a replayable POST page.
    if pg {
        database.execute("CREATE FUNCTION reject_test_label() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'sidebar unavailable'; END $$", &[]).await;
        database.execute("CREATE TRIGGER reject_test_label BEFORE INSERT ON issue_labels FOR EACH ROW EXECUTE FUNCTION reject_test_label()", &[]).await;
    } else {
        database.execute("CREATE TRIGGER reject_test_label BEFORE INSERT ON issue_labels BEGIN SELECT RAISE(ABORT, 'sidebar unavailable'); END", &[]).await;
    }
    let partial = alice
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("repository_id", &repository_id),
                ("title", "Partial creation"),
                ("labels", &ids[0]),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(partial.status, StatusCode::SEE_OTHER, "{}", partial.text());
    let location = partial.location.unwrap();
    assert!(location.starts_with("/alice/proj/issues/4?creation_warning="));
    for _ in 0..2 {
        let (status, html) = alice.page(&location).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Issue created, but some details could not be applied"));
    }
    assert_eq!(
        db::issues::count_repo_issues(database.pool(), repo.id, &Default::default())
            .await
            .unwrap()
            .all,
        4
    );
    database.finish().await;
}

async fn closed_pulls(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    let tip = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &tip).await;
    alice.save("topic", "change.txt", &tip).await;
    let pull = alice.open_pull("Merged history", "topic", "main").await;
    database
        .execute(
            "UPDATE pull_requests SET state='merged', merged=TRUE WHERE id=$1",
            &[pull["id"].as_i64().unwrap()],
        )
        .await;
    let (_, page) = alice.model("/alice/proj/pulls?state=closed").await;
    let Page::Collaboration(page) = page else {
        panic!("not collaboration")
    };
    let issues::Page::List { items, counts, .. } = *page else {
        panic!("not list")
    };
    assert_eq!(counts.closed, 1);
    assert_eq!(counts.open, 0);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].state, "merged");
    database.finish().await;
}

async fn fork_choices(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    alice.save("main", "README.md", "unborn").await;
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let bob = db::users::accounts::get_user_by_username(database.pool(), "bob")
        .await
        .unwrap()
        .unwrap();
    let owner = repo.owner_user_id.unwrap();
    database.execute("INSERT INTO repositories(owner_user_id,name,disk_path,is_fork,forked_from_id,is_private) SELECT owner_user_id,'own-fork',disk_path,TRUE,id,TRUE FROM repositories WHERE id=$1", &[repo.id]).await;
    for n in 1..=102 {
        database.execute(&format!("INSERT INTO repositories(owner_user_id,name,disk_path,is_fork,forked_from_id,is_private) SELECT {},'fork-{n}',disk_path,TRUE,id,{} FROM repositories WHERE id=$1", bob.id, if n == 1 || n == 102 { "TRUE" } else { "FALSE" }), &[repo.id]).await;
    }
    let selected = db::repositories::records::get_repository(database.pool(), "bob", "fork-1")
        .await
        .unwrap()
        .unwrap();
    db::repositories::access::grant_repo_access(
        database.pool(),
        owner,
        selected.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let (status, page) = alice
        .model(&format!("/alice/proj/pulls/new?head_repo={}", selected.id))
        .await;
    assert_eq!(status, StatusCode::OK);
    let Page::Collaboration(page) = page else {
        panic!("not collaboration")
    };
    let issues::Page::New { branches, .. } = *page else {
        panic!("not new pull")
    };
    assert!(branches
        .repositories
        .iter()
        .any(|(_, name)| name == "bob/fork-1"));
    assert!(branches
        .repositories
        .iter()
        .any(|(_, name)| name == "alice/own-fork"));
    assert!(!branches
        .repositories
        .iter()
        .any(|(_, name)| name == "bob/fork-102"));
    database.finish().await;
}

#[tokio::test]
async fn sqlite_fork_choices() {
    fork_choices(false).await;
}
#[tokio::test]
async fn postgres_fork_choices() {
    fork_choices(true).await;
}

#[tokio::test]
async fn sqlite_milestone_models() {
    milestone_models(false).await;
}
#[tokio::test]
async fn postgres_milestone_models() {
    milestone_models(true).await;
}
#[tokio::test]
async fn sqlite_templates_offline() {
    templates_offline(false).await;
}
#[tokio::test]
async fn postgres_templates_offline() {
    templates_offline(true).await;
}
#[tokio::test]
async fn sqlite_label_submissions() {
    label_submissions(false).await;
}
#[tokio::test]
async fn postgres_label_submissions() {
    label_submissions(true).await;
}
#[tokio::test]
async fn sqlite_closed_pulls() {
    closed_pulls(false).await;
}
#[tokio::test]
async fn postgres_closed_pulls() {
    closed_pulls(true).await;
}
