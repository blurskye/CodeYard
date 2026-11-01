use super::{db, json, protection::call, request, Client, Router, Value};
use axum::http::StatusCode;
use codeyard::{
    backend::repositories::access::change_collaborator,
    webui::{app::Page, pages::repos::page::Page as RepoPage},
};

const OP: &str = "repos/list-pull-requests-associated-with-commit";

async fn list(router: &Router, base: &str, token: &str, reference: &str, query: &str) -> Value {
    call(
        router,
        "GET",
        &format!("{base}/commits/{reference}/pulls{query}"),
        token,
        Value::Null,
        (OP, 200),
    )
    .await
}

// Saved size refreshes are asynchronous, even between read-only requests. Schema-check every
// document's nonnegative size, but compare all other fields for equivalent association selectors.
fn stable_documents(mut values: Value) -> Value {
    for pull in values.as_array_mut().unwrap() {
        for side in ["base", "head"] {
            let size = pull[side]["repo"]
                .as_object_mut()
                .unwrap()
                .remove("size")
                .unwrap();
            assert!(size.as_u64().is_some());
        }
    }
    values
}

pub(super) async fn run(
    router: &Router,
    resource: (&db::DbPool, i64, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    session: &Client,
) {
    let (pool, repo, alice, bob) = resource;
    let (writer, reader, outsider) = tokens;
    let root = call(
        router,
        "GET",
        &format!("{base}/git/ref/heads/main"),
        reader,
        Value::Null,
        ("git/get-ref", 200),
    )
    .await["object"]["sha"]
        .as_str()
        .unwrap()
        .to_owned();
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":"refs/heads/association-source","sha":root}),
        ("git/create-ref", 201),
    )
    .await;
    let head = session
        .save_text(
            "association-source",
            "association.txt",
            "published association\n",
            &root,
        )
        .await;
    let native = base.strip_prefix("/api/v3").unwrap();
    let mut pulls = Vec::new();
    for index in 0..12 {
        let branch = if index == 0 {
            "main".to_owned()
        } else {
            format!("association-base-{index}")
        };
        if index > 0 {
            call(
                router,
                "POST",
                &format!("{base}/git/refs"),
                writer,
                json!({"ref":format!("refs/heads/{branch}"),"sha":root}),
                ("git/create-ref", 201),
            )
            .await;
        }
        let value = session.expect("POST",&format!("/api{native}/pulls"),json!({"title":format!("Association <script> {index}"),"head_branch":"association-source","base_branch":branch}),StatusCode::CREATED).await;
        let number = i32::try_from(value["number"].as_i64().unwrap()).unwrap();
        pulls.push(
            db::pull_requests::records::get_pull_request(pool, repo, number)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let values = list(router, base, reader, &head, "?per_page=100").await;
    assert_eq!(values.as_array().unwrap().len(), 12);
    assert_eq!(values[0]["number"], pulls[11].number);
    assert_eq!(values[0]["head"]["sha"], head);
    assert_eq!(values[0]["head"]["repo"]["id"], repo);
    assert_eq!(values[0]["base"]["repo"]["owner"]["login"], "alice");
    assert_eq!(values[0]["author_association"], "OWNER");
    // Owner names resolve in any ASCII case, as on GitHub, so a differently cased owner names
    // the same repository.
    assert_eq!(
        stable_documents(
            list(
                router,
                &base.replace("/alice/", "/ALICE/"),
                reader,
                &head,
                "?per_page=100"
            )
            .await
        ),
        stable_documents(values.clone())
    );
    call(
        router,
        "POST",
        &format!("{base}/git/refs"),
        writer,
        json!({"ref":format!("refs/heads/{root}"),"sha":head}),
        ("git/create-ref", 201),
    )
    .await;
    // Creating the alias changed real repository timestamps; compare subsequent reads of that state.
    let values = list(router, base, reader, &head, "?per_page=100").await;
    // A branch named after an object ID must not retarget an immutable commit URL.
    assert_eq!(list(router, base, reader, &root, "").await, json!([]));
    assert_eq!(
        stable_documents(
            list(
                router,
                base,
                reader,
                &format!("refs%2Fheads%2F{root}"),
                "?per_page=100"
            )
            .await
        ),
        stable_documents(values.clone())
    );
    assert_eq!(
        stable_documents(list(router, base, reader, "association-source", "?per_page=100").await),
        stable_documents(values.clone())
    );
    let (status, headers, page) = request(
        router,
        "GET",
        &format!("{base}/commits/{head}/pulls?per_page=1"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    super::assert_github_schema(OP, 200, &page);
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert_eq!(
        list(router, base, reader, &head, "?per_page=1&page=2").await[0]["number"],
        pulls[10].number
    );
    assert_eq!(
        list(router, base, reader, &head, "?page=100").await,
        json!([])
    );

    let label = db::labels::create_label(
        pool,
        alice,
        repo,
        "area/<script>",
        "ff0000",
        Some("Native label"),
    )
    .await
    .unwrap();
    db::labels::add_pull_request_labels(pool, alice, repo, pulls[0].id, &[label.id])
        .await
        .unwrap();
    db::assignees::assign_user(
        pool,
        alice,
        repo,
        db::assignees::AssignedItem::PullRequest(pulls[0].id),
        alice,
    )
    .await
    .unwrap();
    let milestone = db::milestones::create_milestone(
        pool,
        alice,
        repo,
        "Association release",
        Some("Native milestone"),
        Some("2027-01-02"),
    )
    .await
    .unwrap();
    let before = db::milestones::milestone_document_facts(pool, repo, milestone.id)
        .await
        .unwrap();
    assert_eq!(before.creator_id, Some(alice));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let due_on: chrono::DateTime<chrono::Utc> = "2027-01-02T15:12:31.123Z".parse().unwrap();
    db::milestones::save_milestone(
        pool,
        alice,
        repo,
        Some(milestone.number),
        db::milestones::MilestoneInput {
            title: Some("Updated association release"),
            description: Some(Some("Native milestone")),
            due_on: Some(Some(due_on)),
            state: None,
        },
    )
    .await
    .unwrap();
    let facts = db::milestones::milestone_document_facts(pool, repo, milestone.id)
        .await
        .unwrap();
    assert!(facts.updated_at > before.updated_at);
    db::milestones::set_pull_request_milestone(
        pool,
        alice,
        repo,
        pulls[0].id,
        Some(milestone.number),
    )
    .await
    .unwrap();
    let values = list(router, base, reader, &head, "?per_page=100").await;
    let document = values
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["number"] == pulls[0].number)
        .unwrap();
    assert_eq!(document["labels"][0]["id"], label.id);
    assert_eq!(document["labels"][0]["description"], "Native label");
    assert_eq!(
        document["labels"][0]["color"],
        label.color.trim_start_matches('#')
    );
    assert!(document["labels"][0]["archived_at"].is_null());
    assert!(document["labels"][0]["archived_by"].is_null());
    assert_eq!(document["assignee"]["login"], "alice");
    assert_eq!(
        document["milestone"]["title"],
        "Updated association release"
    );
    assert_eq!(document["milestone"]["creator"]["id"], alice);
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(document["milestone"]["updated_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc),
        facts.updated_at
    );
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(document["milestone"]["due_on"].as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc),
        due_on
    );
    assert_eq!(document["milestone"]["open_issues"], 1);
    let repo_facts = db::repository_stats::repository_document_facts(pool, repo)
        .await
        .unwrap();
    assert_eq!(
        document["base"]["repo"]["open_issues_count"],
        repo_facts.open_items
    );
    assert_eq!(
        document["base"]["repo"]["pushed_at"],
        json!(repo_facts.pushed_at)
    );

    let path = format!("/alice/proj/commit/{head}?view=split");
    let html = session.page_ok(&path).await;
    assert!(html.contains("Associated pull requests"), "{html}");
    // Hydrated SSR and standalone SSR use equivalent numeric and named entities.
    let escaped = html.replace("&#60;", "&lt;").replace("&#62;", "&gt;");
    assert!(escaped.contains("Association &lt;script&gt;"), "{html}");
    assert!(!html.contains("Association <script>"));
    assert!(html.contains("Next pull requests"));
    let (status, model) = session.model(&path).await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(RepoPage::CommitDetail { related_pulls, .. }) = model else {
        panic!("Expected commit detail")
    };
    assert_eq!(related_pulls.rows.len(), 10);
    assert_eq!(related_pulls.rows[0].title, "Association <script> 11");
    assert!(related_pulls.error.is_none());
    let next = related_pulls.next.unwrap();
    assert!(next.contains("view=split") && next.contains("pulls_page=2"));
    let (status, model) = session.model(next.split('#').next().unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(RepoPage::CommitDetail { related_pulls, .. }) = model else {
        panic!("Expected commit detail")
    };
    assert_eq!(related_pulls.rows.len(), 2);
    assert!(related_pulls.previous.is_some() && related_pulls.next.is_none());
    assert_eq!(
        session
            .page(&format!("/alice/proj/commit/{head}?pulls_page=0"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    for token in [Some(outsider), None] {
        let (status, _, body) = request(
            router,
            "GET",
            &format!("{base}/commits/{head}/pulls"),
            token,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        super::assert_github_schema(OP, 404, &body);
    }
    call(
        router,
        "GET",
        &format!("{base}/commits/{}/pulls", "0".repeat(40)),
        reader,
        Value::Null,
        (OP, 404),
    )
    .await;
    call(
        router,
        "GET",
        &format!("{base}/commits/{head}/pulls?page=1002&per_page=100"),
        reader,
        Value::Null,
        (OP, 422),
    )
    .await;
    let before = db::pull_requests::records::get_pull_request(pool, repo, pulls[0].number)
        .await
        .unwrap()
        .unwrap();
    let (status, _, body) = request(
        router,
        "HEAD",
        &format!("{base}/commits/{head}/pulls"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);
    assert_eq!(
        db::pull_requests::records::get_pull_request(pool, repo, pulls[0].number)
            .await
            .unwrap()
            .unwrap()
            .updated_at,
        before.updated_at
    );
    assert_eq!(
        db::milestones::milestone_document_facts(pool, repo, milestone.id)
            .await
            .unwrap()
            .updated_at,
        facts.updated_at
    );

    session.expect("PUT",&format!("/api{native}/pulls/{}",pulls[1].number),json!({"expected_state":"open","expected_updated":pulls[1].updated_at.to_rfc3339(),"state":"closed"}),StatusCode::OK).await;
    assert_eq!(
        list(router, base, reader, &head, "?per_page=100")
            .await
            .as_array()
            .unwrap()
            .len(),
        11
    );
    let current = db::pull_requests::records::get_pull_request(pool, repo, pulls[0].number)
        .await
        .unwrap()
        .unwrap();
    let merged = session
        .expect(
            "POST",
            &format!("/api{native}/pulls/{}/merge", current.number),
            json!({"expected_head":head,"expected_base":root}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(merged["merged"], true);
    let values = list(router, base, reader, &head, "?per_page=100").await;
    assert_eq!(values.as_array().unwrap().len(), 1);
    assert_eq!(values[0]["number"], pulls[0].number);
    assert_eq!(values[0]["state"], "closed");
    assert_eq!(
        list(router, base, reader, merged["sha"].as_str().unwrap(), "").await[0]["number"],
        pulls[0].number
    );
    assert_eq!(
        list(router, base, reader, "association-source", "?per_page=100")
            .await
            .as_array()
            .unwrap()
            .len(),
        11
    );
    assert_eq!(list(router, base, reader, &root, "").await, json!([]));
    let html = session.page_ok(&path).await;
    assert!(html.contains("Merged"), "{html}");

    // The fork from the comparison fixture is private and independently authorized.
    let fork = db::repositories::records::get_repository(pool, "bob", "proj")
        .await
        .unwrap()
        .unwrap();
    change_collaborator(
        pool,
        alice,
        repo,
        "bob",
        Some(db::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    // A read collaborator is a collaborator on issues and pull requests too, as on GitHub.
    let record = db::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codeyard::backend::repositories::access::author_association(pool, &record, bob)
            .await
            .unwrap(),
        "COLLABORATOR"
    );
    let forker = Client::login(router, pool, bob).await;
    let foreign = forker.expect("POST",&format!("/api{native}/pulls"),json!({"title":"Private source association","head_repo_id":fork.id,"head_branch":"main","base_branch":"main"}),StatusCode::CREATED).await;
    let foreign_sha = foreign["head_sha"].as_str().unwrap();
    let values = list(router, base, outsider, foreign_sha, "").await;
    assert_eq!(values[0]["head"]["repo"]["id"], fork.id);
    assert_eq!(values[0]["head"]["repo"]["private"], true);
    assert_eq!(values[0]["author_association"], "COLLABORATOR");
    // The pull request is alice's to see, but its private source repository is not: the head
    // names its owner and leaves the repository out, as GitHub does, and nothing names the fork.
    let redacted = list(router, base, reader, foreign_sha, "").await;
    assert_eq!(redacted[0]["number"], foreign["number"]);
    assert_eq!(
        redacted[0]["head"],
        json!({"label":"bob:main","ref":"main","sha":foreign_sha,"user":values[0]["head"]["user"],"repo":null})
    );
    assert_eq!(redacted[0]["head"]["user"]["login"], "bob");
    assert!(!redacted.to_string().contains("bob/proj"), "{redacted}");
    // The pull request itself is shown with the same head, so `gh pr view` works where
    // `gh pr list` does.
    let (status, _, pull) = request(
        router,
        "GET",
        &format!("{base}/pulls/{}", foreign["number"]),
        Some(reader),
        Value::Null,
    )
    .await;
    assert_eq!(status.as_u16(), 200, "{pull}");
    assert_eq!(pull["number"], foreign["number"]);
    assert_eq!(pull["head"], redacted[0]["head"]);
    assert!(!pull.to_string().contains("bob/proj"), "{pull}");
    assert!(session
        .page_ok(&format!("/alice/proj/commit/{foreign_sha}"))
        .await
        .contains("Private source association"));
    change_collaborator(
        pool,
        bob,
        fork.id,
        "alice",
        Some(db::models::AccessLevel::Read),
    )
    .await
    .unwrap();
    assert_eq!(
        list(router, base, reader, foreign_sha, "").await[0]["head"]["repo"]["id"],
        fork.id
    );
    change_collaborator(pool, bob, fork.id, "alice", None)
        .await
        .unwrap();
    change_collaborator(pool, alice, repo, "bob", None)
        .await
        .unwrap();
}
