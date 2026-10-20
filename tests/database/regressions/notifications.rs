use super::*;

#[tokio::test]
async fn refollows_keep_a_durable_per_actor_notification_cooldown() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let actor = f.actor("follower").await;
        let target = f.actor("recipient").await;
        let broadcaster = notifications::NotificationBroadcaster::new(f.pool());
        for _ in 0..5 {
            identity::following::change(f.pool(), &broadcaster, actor, "recipient", true)
                .await
                .unwrap();
            identity::following::change(f.pool(), &broadcaster, actor, "recipient", false)
                .await
                .unwrap();
        }
        let rows =
            db::notifications::records::list_notifications_for_user(f.pool(), target, 100, false)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        f.execute(
            "UPDATE notifications SET created_at='2000-01-01T00:00:00.000000Z' WHERE user_id=$1",
            &[target],
        )
        .await;
        identity::following::change(f.pool(), &broadcaster, actor, "recipient", true)
            .await
            .unwrap();
        assert_eq!(
            db::notifications::records::list_notifications_for_user(f.pool(), target, 100, false)
                .await
                .unwrap()
                .len(),
            2
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn notification_batches_keep_order_visibility_and_continuation() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("inbox_owner").await;
        let reader = f.actor("inbox_reader").await;
        let visible = repo(&f, owner, "visible").await;
        let hidden = repo(&f, owner, "hidden").await;
        db::repositories::access::grant_repo_access(
            f.pool(),
            reader,
            visible.id,
            AccessLevel::Read,
        )
        .await
        .unwrap();
        let mut expected = Vec::new();
        for index in 0..205 {
            let row = notice(
                f.pool(),
                reader,
                Some(visible.id),
                &format!("notice-{index}"),
            )
            .await;
            expected.push(row.id);
            if index % 3 == 0 {
                notice(f.pool(), reader, Some(hidden.id), "hidden").await;
            }
        }
        expected.reverse();
        let session = session(f.pool(), reader).await;
        let mut before = None;
        let mut listed = Vec::new();
        loop {
            let page = db::notifications::records::notification_inbox(
                f.pool(),
                &session,
                reader,
                NotificationInboxAction::List {
                    filter: NotificationFilter::inbox(false),
                    limit: 100,
                    before,
                },
            )
            .await
            .unwrap()
            .unwrap();
            assert!(page.notifications.len() <= 100);
            assert_eq!(page.unread_count, 205);
            listed.extend(page.notifications.iter().map(|n| n.id));
            before = page.next_cursor;
            if before.is_none() {
                break;
            }
        }
        assert_eq!(listed, expected);
        assert_eq!(
            db::notifications::records::count_header_unread(f.pool(), reader)
                .await
                .unwrap(),
            205
        );
        db::notifications::records::mark_all_notifications_read(f.pool(), reader)
            .await
            .unwrap();
        assert_eq!(
            db::notifications::records::count_header_unread(f.pool(), reader)
                .await
                .unwrap(),
            0
        );
        db::repositories::access::revoke_repo_access(f.pool(), reader, visible.id)
            .await
            .unwrap();
        assert_eq!(
            db::notifications::records::count_header_unread(f.pool(), reader)
                .await
                .unwrap(),
            0
        );
        f.finish().await;
    }
}
