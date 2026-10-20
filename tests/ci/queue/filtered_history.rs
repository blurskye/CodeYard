use super::*;

async fn contract(pg: bool) {
    let mut db = Database::new(pg).await;
    let actor = db.actor("filtered_owner").await;
    let outsider = db.actor("filtered_outsider").await;
    let repo = db.repo(actor, "history").await;
    let oldest = db.history(repo, actor).await;
    let newer = db.history(repo, actor).await;
    for id in [oldest, newer] {
        db.execute("UPDATE ci_runs SET status='failed', branch='refs/heads/topic', workflow_path='.github/workflows/ci.yml,.github/workflows/deploy.yml' WHERE id=$1", &[id]).await;
    }
    for _ in 0..201 {
        db.history(repo, actor).await;
    }
    let mut filter = db::ci::CiRunFilter {
        status: "failed".into(),
        branch: "topic".into(),
        event: "push".into(),
        workflow: WORKFLOW.into(),
        text: "HISTORY".into(),
        ..Default::default()
    };
    for scope in [Some(repo), None] {
        let runs = db::ci::list_filtered_ci_runs(db.pool(), actor, scope, &filter, 1)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, newer);
        filter.before = Some(newer);
        let runs = db::ci::list_filtered_ci_runs(db.pool(), actor, scope, &filter, 1)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, oldest);
        filter.before = None;
        filter.text = runs[0].run_id.to_string();
        assert_eq!(
            db::ci::list_filtered_ci_runs(db.pool(), actor, scope, &filter, 200)
                .await
                .unwrap()[0]
                .id,
            oldest
        );
        filter.text = "HISTORY".into();
    }
    assert!(
        db::ci::list_filtered_ci_runs(db.pool(), outsider, Some(repo), &filter, 200)
            .await
            .unwrap()
            .is_empty()
    );
    filter.event = "release".into();
    assert!(
        db::ci::list_filtered_ci_runs(db.pool(), actor, Some(repo), &filter, 200)
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await;
}

#[tokio::test]
async fn sqlite_filters_precede_limit_and_paginate_matches() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_filters_precede_limit_and_paginate_matches() {
    contract(true).await;
}
