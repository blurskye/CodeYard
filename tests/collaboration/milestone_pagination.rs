use crate::support::{database::Database, fake_webui_bundle as bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::{
    backend::{db, repositories::lifecycle},
    webui::{app::Page, pages::repos::issues::Page as IssuePage},
};

async fn milestone_items_have_reachable_continuation_pages(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let milestone =
        db::milestones::create_milestone(f.pool(), owner, repository.id, "Release", None, None)
            .await
            .unwrap();
    for index in 0..101 {
        let issue = db::issues::create_issue(
            f.pool(),
            repository.id,
            owner,
            &format!("Issue {index}"),
            "",
        )
        .await
        .unwrap();
        db::milestones::set_issue_milestone(
            f.pool(),
            owner,
            repository.id,
            issue.id,
            Some(milestone.number),
        )
        .await
        .unwrap();
    }
    for index in 0..31 {
        let pull = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: repository.id,
                head_repo_id: Some(repository.id),
                author_id: owner,
                title: &format!("Pull {index}"),
                body: "",
                head_branch: &format!("topic-{index}"),
                head_sha: &"a".repeat(40),
                base_branch: "main",
                base_sha: &"b".repeat(40),
                is_draft: false,
            },
        )
        .await
        .unwrap();
        db::milestones::set_pull_request_milestone(
            f.pool(),
            owner,
            repository.id,
            pull.id,
            Some(milestone.number),
        )
        .await
        .unwrap();
    }
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), owner).await;
    let mut issues = Vec::new();
    let mut pulls = Vec::new();
    for page in 1..=4 {
        let path = format!("/alice/proj/milestones/1?state=all&page={page}");
        let (status, model) = client.model(&path).await;
        assert_eq!(status, StatusCode::OK);
        let Page::Collaboration(model) = model else {
            panic!("collaboration")
        };
        let IssuePage::MilestoneDetail {
            milestone,
            items,
            pull_requests,
            pagination,
            ..
        } = *model
        else {
            panic!("milestone")
        };
        assert_eq!(milestone.open_items, 132);
        assert_eq!(pagination, (page, page < 4));
        issues.extend(items.into_iter().map(|i| i.number));
        pulls.extend(pull_requests.into_iter().map(|i| i.number));
        let (status, html) = client.page(&path).await;
        assert_eq!(status, StatusCode::OK);
        let html = html
            .replace("&#38;", "&")
            .replace("&#x26;", "&")
            .replace("&amp;", "&");
        assert_eq!(
            html.contains(&format!(
                "href=\"/alice/proj/milestones/1?state=all&page={}\"",
                page + 1
            )),
            page < 4,
            "{html}"
        );
        if page > 1 {
            assert!(
                html.contains(&format!(
                    "href=\"/alice/proj/milestones/1?state=all&page={}\"",
                    page - 1
                )),
                "{html}"
            );
        }
    }
    issues.sort_unstable();
    issues.dedup();
    pulls.sort_unstable();
    pulls.dedup();
    assert_eq!(issues.len(), 101);
    assert_eq!(pulls.len(), 31);
    assert_eq!(
        client.page("/alice/proj/milestones/1?page=0").await.0,
        StatusCode::BAD_REQUEST
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_milestone_items_have_reachable_continuation_pages() {
    milestone_items_have_reachable_continuation_pages(false).await;
}

#[tokio::test]
async fn postgres_milestone_items_have_reachable_continuation_pages() {
    milestone_items_have_reachable_continuation_pages(true).await;
}
