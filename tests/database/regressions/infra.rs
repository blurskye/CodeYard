use super::*;

#[tokio::test]
async fn codeowners_does_not_expand_secret_teams_even_for_repository_writers() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("codeowner_owner").await;
        let member = f.actor("codeowner_member").await;
        organization(&f, 1001, member).await;
        f.execute("INSERT INTO teams(id,org_id,slug,name,privacy) VALUES(10001,1001,'owners','Owners','secret')", &[]).await;
        f.execute(
            "INSERT INTO team_memberships(team_id,user_id,role) VALUES(10001,$1,'member')",
            &[member],
        )
        .await;
        let r = repo(&f, owner, "codeowners").await;
        // Only the organization that owns the repository can have its teams expanded.
        f.execute(
            "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=1001 WHERE id=$1",
            &[r.id],
        )
        .await;
        let r = db::repositories::records::get_repository_by_id(f.pool(), r.id)
            .await
            .unwrap()
            .unwrap();
        db::repositories::access::grant_repo_access(f.pool(), member, r.id, AccessLevel::Write)
            .await
            .unwrap();
        let resolve = || {
            codeyard::backend::repositories::code_owners::resolve_owners(
                f.pool(),
                &r,
                ["@org1001/owners"],
            )
        };
        assert!(resolve().await.unwrap()["@org1001/owners"].is_empty());
        f.execute("UPDATE teams SET privacy='visible' WHERE id=10001", &[])
            .await;
        assert_eq!(resolve().await.unwrap()["@org1001/owners"], vec![member]);
        f.finish().await;
    }
}

#[tokio::test]
async fn retargeting_a_pull_request_excludes_checks_for_its_previous_target() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("retarget_owner").await;
        let r = repo(&f, owner, "retarget").await;
        let node = db::cluster::register_own_node(
            f.pool(),
            &"ab".repeat(32),
            "ci-retention",
            &["storage".into()],
            "test",
        )
        .await
        .unwrap();
        assert!(db::cluster::place_repository(f.pool(), r.id, node.id)
            .await
            .unwrap());
        let sha = "a".repeat(40);
        let pull = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: r.id,
                head_repo_id: Some(r.id),
                author_id: owner,
                title: "Retarget",
                body: "",
                head_branch: "topic",
                head_sha: &sha,
                base_branch: "main",
                base_sha: &"b".repeat(40),
                is_draft: false,
            },
        )
        .await
        .unwrap();
        let path = ".github/workflows/check.yml";
        let run = db::ci::enqueue_ci_run(
            f.pool(),
            db::ci::NewCiRun {
                repo_id: r.id,
                triggered_by_id: owner,
                pipeline: "ci",
                event: "pull_request",
                pull_request_id: Some(pull.id),
                branch: "main",
                head_sha: &sha,
                workflow_path: path,
                workflow_blobs: &serde_json::json!({(path): "c".repeat(40)}),
                workflow_inputs: None,
                environment: None,
                manual: true,
            },
            &[owner],
        )
        .await
        .unwrap();
        assert_eq!(
            db::cluster::get_write_version(f.pool(), r.id)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            db::ci::retained_ci_commits(f.pool(), r.id).await.unwrap(),
            vec![sha.clone()]
        );
        f.execute("UPDATE ci_runs SET status='success' WHERE id=$1", &[run.id])
            .await;
        assert_eq!(
            db::ci::latest_ci_runs_for_commit(f.pool(), r.id, &sha, pull.id, "main")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db::ci::list_pull_request_check_statuses(f.pool(), &[pull.id])
                .await
                .unwrap(),
            vec![(pull.id, "success".into())]
        );
        f.execute(
            "UPDATE pull_requests SET base_branch='release' WHERE id=$1",
            &[pull.id],
        )
        .await;
        assert!(
            db::ci::latest_ci_runs_for_commit(f.pool(), r.id, &sha, pull.id, "release")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db::ci::list_pull_request_check_statuses(f.pool(), &[pull.id])
                .await
                .unwrap()
                .is_empty()
        );
        // A run on the current target is eligible; an unrelated PR is still not.
        f.execute("UPDATE ci_runs SET branch='release' WHERE id=$1", &[run.id])
            .await;
        assert_eq!(
            db::ci::latest_ci_runs_for_commit(f.pool(), r.id, &sha, pull.id, "release")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db::ci::list_pull_request_check_statuses(f.pool(), &[pull.id])
                .await
                .unwrap()
                .len(),
            1
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn native_notification_projections_refresh_links_before_returning() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("link_owner").await;
        let reader = f.actor("link_reader").await;
        let r = repo(&f, owner, "old_name").await;
        db::repositories::access::grant_repo_access(f.pool(), reader, r.id, AccessLevel::Read)
            .await
            .unwrap();
        let session = session(f.pool(), reader).await;
        let n = db::notifications::records::create_notification(
            f.pool(),
            NewNotification {
                user_id: reader,
                actor_id: Some(owner),
                repo_id: Some(r.id),
                kind: "mention",
                title: "Mention",
                body: None,
                link: Some("/link_owner/old_name/issues/1#comment-2"),
                metadata: &serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        records::rename_repository(f.pool(), owner, r.id, "new_name")
            .await
            .unwrap();
        let inbox = db::notifications::records::notification_inbox(
            f.pool(),
            &session,
            reader,
            NotificationInboxAction::List {
                filter: NotificationFilter::default(),
                limit: 10,
                before: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let delivery = db::notifications::records::notification_delivery(
            f.pool(),
            &session,
            reader,
            Some(n.id),
        )
        .await
        .unwrap()
        .unwrap();
        let expected = Some("/link_owner/new_name/issues/1#comment-2");
        assert_eq!(inbox.notifications[0].link.as_deref(), expected);
        assert_eq!(
            delivery.notification.as_ref().unwrap().link.as_deref(),
            expected
        );
        db::repositories::access::revoke_repo_access(f.pool(), reader, r.id)
            .await
            .unwrap();
        records::rename_repository(f.pool(), owner, r.id, "hidden_name")
            .await
            .unwrap();
        assert_eq!(inbox.notifications[0].link.as_deref(), expected);
        assert_eq!(delivery.notification.unwrap().link.as_deref(), expected);
        assert!(db::notifications::records::notification_delivery(
            f.pool(),
            &session,
            reader,
            Some(n.id)
        )
        .await
        .unwrap()
        .unwrap()
        .notification
        .is_none());
        f.finish().await;
    }
}

#[tokio::test]
async fn webhook_settings_reads_recheck_repository_and_organization_authority() {
    use db::webhooks::{self, HookOwner, WebhookFields};
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("hook_owner").await;
        let admin = f.actor("hook_admin").await;
        let r = repo(&f, owner, "hooks").await;
        organization(&f, 1001, owner).await;
        f.execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'owner')",
            &[admin],
        )
        .await;
        db::repositories::access::grant_repo_access(f.pool(), admin, r.id, AccessLevel::Admin)
            .await
            .unwrap();
        for scope in [HookOwner::Repository(r.id), HookOwner::Organization(1001)] {
            let hook = webhooks::insert_webhook(
                f.pool(),
                scope,
                owner,
                &WebhookFields {
                    payload_url: "https://example.com/hook".into(),
                    content_type: "application/json".into(),
                    events: "push".into(),
                    active: true,
                    ssl_verify: true,
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
            webhooks::insert_delivery(
                f.pool(),
                hook,
                &webhooks::NewDelivery {
                    guid: uuid::Uuid::new_v4().to_string(),
                    event: "push".into(),
                    action: None,
                    status_code: Some(200),
                    duration_ms: 1,
                    error: None,
                    request_headers: "".into(),
                    request_body: "private payload".into(),
                    response_headers: None,
                    response_body: None,
                },
            )
            .await
            .unwrap();
            let id = webhooks::list_deliveries(f.pool(), hook).await.unwrap()[0].id;
            let snapshot = webhooks::read_settings(f.pool(), scope, admin, Some(hook), Some(id), 0)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                snapshot.delivery.as_ref().unwrap().request_body,
                "private payload"
            );
            assert_eq!(snapshot.hooks.len(), 1);
            assert_eq!(snapshot.deliveries.len(), 1);
            match scope {
                HookOwner::Repository(_) => {
                    assert!(
                        db::repositories::access::revoke_repo_access(f.pool(), admin, r.id)
                            .await
                            .unwrap()
                    );
                }
                HookOwner::Organization(_) => {
                    f.execute(
                        "UPDATE org_memberships SET role='member' WHERE org_id=1001 AND user_id=$1",
                        &[admin],
                    )
                    .await
                }
            }
            assert!(
                webhooks::read_settings(f.pool(), scope, admin, Some(hook), Some(id), 0)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        f.finish().await;
    }
}
