use super::*;

#[tokio::test]
async fn privileged_node_and_crawler_changes_require_the_original_live_session() {
    use db::cluster::Authority;
    use db::models::NodeStatusChange;
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let admin = f.actor("session_admin").await;
        let other = f.actor("session_other").await;
        f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
            .await;
        db::node_config::upsert_node_configuration(f.pool(), "public", true)
            .await
            .unwrap();
        let key = codeyard::backend::cluster::NodeKey::generate().public_hex();
        db::cluster::register_own_node(
            f.pool(),
            &key,
            "session-node",
            &["storage".to_owned()],
            "test",
        )
        .await
        .unwrap();
        let original = session(f.pool(), admin).await;
        let replacement = session(f.pool(), admin).await;
        let foreign = session(f.pool(), other).await;
        assert!(
            db::node_config::set_ai_crawlers(f.pool(), admin, &original, "allow")
                .await
                .unwrap()
        );
        assert!(matches!(
            db::cluster::set_node_status(
                f.pool(),
                Authority::Administrator {
                    user_id: admin,
                    session: &original
                },
                "session-node",
                "revoked"
            )
            .await
            .unwrap(),
            NodeStatusChange::Changed { .. }
        ));
        db::users::sessions::delete_session(f.pool(), &original)
            .await
            .unwrap();
        for invalid in [&original, &foreign] {
            assert!(
                !db::node_config::set_ai_crawlers(f.pool(), admin, invalid, "block")
                    .await
                    .unwrap()
            );
            assert_eq!(
                db::cluster::set_node_status(
                    f.pool(),
                    Authority::Administrator {
                        user_id: admin,
                        session: invalid
                    },
                    "session-node",
                    "active"
                )
                .await
                .unwrap(),
                NodeStatusChange::Denied
            );
        }
        // A second login must not substitute for the revoked session, but remains usable itself.
        assert!(
            db::node_config::set_ai_crawlers(f.pool(), admin, &replacement, "block")
                .await
                .unwrap()
        );
        f.execute(
            "UPDATE sessions SET expires_at='2000-01-01T00:00:00.000000Z' WHERE user_id=$1",
            &[admin],
        )
        .await;
        assert!(
            !db::node_config::set_ai_crawlers(f.pool(), admin, &replacement, "allow")
                .await
                .unwrap()
        );
        assert_eq!(
            db::cluster::set_node_status(
                f.pool(),
                Authority::Administrator {
                    user_id: admin,
                    session: &replacement
                },
                "session-node",
                "active"
            )
            .await
            .unwrap(),
            NodeStatusChange::Denied
        );
        assert!(matches!(
            db::cluster::set_node_status(f.pool(), Authority::Operator, "session-node", "active")
                .await
                .unwrap(),
            NodeStatusChange::Changed { .. }
        ));
        f.finish().await;
    }
}

#[tokio::test]
async fn late_pr_comment_notices_and_mentions_cannot_restore_deleted_text() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("comment_owner").await;
        let r = repo(&f, owner, "comments").await;
        let foreign = repo(&f, owner, "foreign_comments").await;
        let pr = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: r.id,
                head_repo_id: None,
                author_id: owner,
                title: "PR",
                body: "",
                head_branch: "topic",
                head_sha: &"a".repeat(40),
                base_branch: "main",
                base_sha: &"b".repeat(40),
                is_draft: false,
            },
        )
        .await
        .unwrap();
        let comment = db::pull_requests::reviews::add_pull_request_comment(
            f.pool(),
            db::pull_requests::reviews::NewPullRequestComment {
                pr_id: pr.id,
                author_id: owner,
                body: "deleted text",
                path: None,
                line: None,
                side: None,
                commit_sha: None,
                in_reply_to_comment_id: None,
                expected_revisions: None,
            },
        )
        .await
        .unwrap();
        for deleted in [false, true] {
            if deleted {
                assert!(db::pull_requests::reviews::delete_pull_request_comment(
                    f.pool(),
                    owner,
                    r.id,
                    pr.id,
                    comment.id
                )
                .await
                .unwrap());
            }
            for (kind, field) in [
                ("pull_request_comment", "comment_id"),
                ("pull_request_comment_reply", "comment_id"),
                ("mention", "pull_request_comment_id"),
            ] {
                for scope in [r.id, foreign.id] {
                    let metadata = serde_json::json!({(field): comment.id});
                    let result = db::notifications::records::create_notification(
                        f.pool(),
                        NewNotification {
                            user_id: owner,
                            actor_id: Some(owner),
                            repo_id: Some(scope),
                            kind,
                            title: "Comment",
                            body: Some("deleted text"),
                            link: None,
                            metadata: &metadata,
                        },
                    )
                    .await;
                    if !deleted && scope == r.id {
                        result.unwrap();
                    } else {
                        assert!(result
                            .unwrap_err()
                            .is::<db::notifications::records::NotificationSuppressed>());
                    }
                }
            }
        }
        assert!(db::notifications::records::list_notifications_for_user(
            f.pool(),
            owner,
            100,
            false
        )
        .await
        .unwrap()
        .is_empty());
        f.finish().await;
    }
}

#[tokio::test]
async fn delivery_batches_scopes_and_keeps_requested_read_history() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("delivery_owner").await;
        f.execute("WITH RECURSIVE n(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM n WHERE n<1000)
            INSERT INTO repositories(owner_kind,owner_user_id,name,disk_path,is_private,created_by_id)
            SELECT 'user',$1,'scope-'||n,'/fixture/'||n,TRUE,$1 FROM n", &[owner]).await;
        f.execute("INSERT INTO notifications(user_id,repo_id,scope_repository_id,kind,title,body)
            SELECT $1,id,id,'issue_opened','Notice','body' FROM repositories WHERE owner_user_id=$1", &[owner]).await;
        let original = session(f.pool(), owner).await;
        let delivery = tokio::time::timeout(
            Duration::from_secs(5),
            db::notifications::records::notification_delivery(f.pool(), &original, owner, None),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(delivery.unread_count, 1000);
        let rows =
            db::notifications::records::list_notifications_for_user(f.pool(), owner, 1, false)
                .await
                .unwrap();
        let requested = rows[0].id;
        f.execute("UPDATE notifications SET read_at=CURRENT_TIMESTAMP,done_at=CURRENT_TIMESTAMP WHERE user_id=$1", &[owner]).await;
        let unread = notice(f.pool(), owner, None, "Unread account notice").await;
        let delivery = db::notifications::records::notification_delivery(
            f.pool(),
            &original,
            owner,
            Some(requested),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(delivery.unread_count, 1);
        assert_eq!(delivery.notification.unwrap().id, requested);
        let delivery = db::notifications::records::notification_delivery(
            f.pool(),
            &original,
            owner,
            Some(unread.id),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(delivery.notification.unwrap().id, unread.id);
        f.finish().await;
    }
}
