use super::*;

#[tokio::test]
async fn organization_assignees_require_scoped_grants_not_public_visibility() {
    use db::assignees::{self, AssignedItem, AssignmentRefusal};
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("assignment_owner").await;
        let admin = f.actor("assignment_admin").await;
        let writer = f.actor("assignment_writer").await;
        let member = f.actor("assignment_member").await;
        let outsider = f.actor("assignment_outsider").await;
        organization(&f, 1001, owner).await;
        organization(&f, 1002, outsider).await;
        let r = org_repo(&f, owner, 1001, "assignments").await;
        f.execute(
            "UPDATE repositories SET is_private=FALSE WHERE id=$1",
            &[r.id],
        )
        .await;
        for (user, role) in [(admin, "admin"), (writer, "member"), (member, "member")] {
            f.execute(
                &format!(
                    "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'{role}')"
                ),
                &[user],
            )
            .await;
        }
        for (team, org, user) in [(2001, 1001, writer), (2002, 1002, outsider)] {
            f.execute(&format!("INSERT INTO teams(id,org_id,name,slug) VALUES($1,$2,'team{team}','team{team}')"), &[team, org]).await;
            f.execute(
                "INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,'member')",
                &[team, user],
            )
            .await;
            f.execute(
                "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,3)",
                &[team, r.id],
            )
            .await;
        }
        let issue = db::issues::create_issue(f.pool(), r.id, owner, "Assign me", "")
            .await
            .unwrap();
        let expected = ["assignment_admin", "assignment_owner", "assignment_writer"];
        assert_eq!(
            assignees::list_assignable_usernames(f.pool(), r.id)
                .await
                .unwrap(),
            expected
        );
        for user in [owner, admin, writer] {
            assert!(assignees::assign_user(
                f.pool(),
                owner,
                r.id,
                AssignedItem::Issue(issue.id),
                user
            )
            .await
            .unwrap());
        }
        for user in [member, outsider] {
            let error =
                assignees::assign_user(f.pool(), owner, r.id, AssignedItem::Issue(issue.id), user)
                    .await
                    .unwrap_err();
            assert_eq!(
                error.downcast_ref::<AssignmentRefusal>(),
                Some(&AssignmentRefusal::Unassignable)
            );
        }
        f.execute("DELETE FROM team_repo_access WHERE team_id=2001", &[])
            .await;
        assert!(!assignees::list_assignable_usernames(f.pool(), r.id)
            .await
            .unwrap()
            .contains(&"assignment_writer".to_owned()));
        assert!(assignees::assign_user(
            f.pool(),
            owner,
            r.id,
            AssignedItem::Issue(issue.id),
            writer
        )
        .await
        .is_err());
        f.finish().await;
    }
}

#[tokio::test]
async fn repository_filters_share_native_search_semantics_and_escape_literals() {
    use db::repositories::records::{RepositoryFilter, RepositorySort};
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("search_owner").await;
        let reader = f.actor("search_reader").await;
        let r = records::create_repository(
            f.pool(),
            owner,
            "École",
            Some("CAFÉ notes 100%_literal"),
            false,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        db::repositories::access::grant_repo_access(f.pool(), reader, r.id, AccessLevel::Read)
            .await
            .unwrap();
        // Exact Unicode names work even with PostgreSQL's C collation. SQLite additionally
        // uses the same Unicode-insensitive matcher as its ordinary repository search.
        let mut queries = vec!["École", "CAFÉ", "100%_literal"];
        if !pg {
            queries.extend(["école", "café"]);
        }
        for q in queries {
            let (owned, count) =
                records::list_user_repository_page(f.pool(), owner, false, q, 30, 0)
                    .await
                    .unwrap();
            assert_eq!(count, 1, "owned {pg} {q}");
            assert_eq!(owned[0].id, r.id);
            let (shared, count) = records::list_shared_repository_page(f.pool(), reader, q, 30, 0)
                .await
                .unwrap();
            assert_eq!(count, 1, "shared {pg} {q}");
            assert_eq!(shared[0].repository.id, r.id);
            let public = records::search_public_repositories(
                f.pool(),
                q,
                RepositoryFilter::default(),
                RepositorySort::Recent,
                30,
                0,
            )
            .await
            .unwrap();
            assert_eq!(public.iter().map(|r| r.id).collect::<Vec<_>>(), [r.id]);
        }
        for q in ["missing", "100.*", "100%Xliteral"] {
            assert_eq!(
                records::list_user_repository_page(f.pool(), owner, false, q, 30, 0)
                    .await
                    .unwrap()
                    .1,
                0
            );
            assert_eq!(
                records::list_shared_repository_page(f.pool(), reader, q, 30, 0)
                    .await
                    .unwrap()
                    .1,
                0
            );
            assert!(records::search_public_repositories(
                f.pool(),
                q,
                RepositoryFilter::default(),
                RepositorySort::Recent,
                30,
                0
            )
            .await
            .unwrap()
            .is_empty());
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn archived_deletion_retains_account_and_organization_restrictions() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("delete_owner").await;
        let other = f.actor("delete_other").await;
        organization(&f, 1001, owner).await;
        for org in [false, true] {
            let r = if org {
                org_repo(&f, owner, 1001, "archived_org").await
            } else {
                repo(&f, owner, "archived_personal").await
            };
            records::set_repository_archived(f.pool(), owner, r.id, true)
                .await
                .unwrap();
            assert!(records::delete_repository(f.pool(), other, r.id)
                .await
                .is_err());
            f.execute(
                "UPDATE users SET must_change_password=TRUE WHERE id=$1",
                &[owner],
            )
            .await;
            assert!(records::delete_repository(f.pool(), owner, r.id)
                .await
                .is_err());
            f.execute(
                "UPDATE users SET must_change_password=FALSE WHERE id=$1",
                &[owner],
            )
            .await;
            if org {
                f.execute(
                    "INSERT INTO org_memberships(org_id,user_id,role) VALUES(1001,$1,'admin')",
                    &[other],
                )
                .await;
                f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
                    .await;
                assert!(records::delete_repository(f.pool(), other, r.id)
                    .await
                    .is_err());
                f.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[owner])
                    .await;
            }
            assert!(
                records::delete_repository(f.pool(), owner, r.id)
                    .await
                    .unwrap()
                    .is_archived
            );
            assert!(records::get_repository_by_id(f.pool(), r.id)
                .await
                .unwrap()
                .is_none());
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn milestone_list_filters_omit_descriptions_but_keep_counts_and_dates() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("filter_owner").await;
        let r = repo(&f, owner, "filters").await;
        let description = "x".repeat(65_536);
        let m = db::milestones::create_milestone(
            f.pool(),
            owner,
            r.id,
            "First",
            Some(&description),
            Some("2030-01-01"),
        )
        .await
        .unwrap();
        let issue = db::issues::create_issue(f.pool(), r.id, owner, "Open", "")
            .await
            .unwrap();
        db::milestones::set_issue_milestone(f.pool(), owner, r.id, issue.id, Some(m.number))
            .await
            .unwrap();
        let rows = db::milestones::list_milestone_filters(f.pool(), r.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].milestone.description.is_none());
        assert_eq!(rows[0].milestone.id, m.id);
        assert_eq!(rows[0].milestone.due_date.as_deref(), Some("2030-01-01"));
        assert_eq!(rows[0].open_items, 1);
        assert_eq!(rows[0].closed_items, 0);
        assert_eq!(
            db::milestones::get_milestone(f.pool(), r.id, m.number)
                .await
                .unwrap()
                .unwrap()
                .milestone
                .description
                .as_deref(),
            Some(description.as_str())
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn activity_scan_exposes_exhaustion_even_without_visible_rows() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("scan_owner").await;
        let batch = db::activity::list_user_activity(f.pool(), owner, None, 2, 0)
            .await
            .unwrap();
        let (rows, exhausted) = (batch.rows, batch.scanned < 2);
        assert!(rows.is_empty() && exhausted);
        for name in ["hidden_one", "hidden_two", "hidden_three"] {
            repo(&f, owner, name).await;
        }
        let batch = db::activity::list_user_activity(f.pool(), owner, None, 2, 0)
            .await
            .unwrap();
        let (rows, exhausted) = (batch.rows, batch.scanned < 2);
        assert!(rows.is_empty() && !exhausted);
        let batch = db::activity::list_user_activity(f.pool(), owner, None, 2, 2)
            .await
            .unwrap();
        let (rows, exhausted) = (batch.rows, batch.scanned < 2);
        assert!(rows.is_empty() && exhausted);
        let batch = db::activity::list_user_activity(f.pool(), owner, Some(owner), 10, 0)
            .await
            .unwrap();
        let (rows, exhausted) = (batch.rows, batch.scanned < 10);
        assert_eq!(rows.len(), 3);
        assert!(exhausted);
        f.finish().await;
    }
}

#[tokio::test]
async fn cleanup_label_placement_activity_and_quota_lookups_have_index_paths() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        for (sql, index) in [
            ("SELECT id FROM repositories WHERE storage_node_id=1 ORDER BY id", "idx_repositories_storage_node"),
            ("SELECT id FROM users WHERE avatar_file_id='00000000-0000-0000-0000-000000000001'", "idx_users_avatar_file"),
            ("SELECT id FROM organizations WHERE avatar_file_id='00000000-0000-0000-0000-000000000001'", "idx_organizations_avatar_file"),
            ("SELECT id FROM release_assets WHERE file_id='00000000-0000-0000-0000-000000000001'", "idx_release_assets_file_id"),
            ("SELECT COUNT(*) FROM issue_labels WHERE label_id=1", "idx_issue_labels_label"),
            ("SELECT COUNT(*) FROM pr_labels WHERE label_id=1", "idx_pr_labels_label"),
            ("SELECT id FROM pull_requests WHERE merged_by_id=1 AND merged_at IS NOT NULL", "idx_pull_requests_merged_by"),
            ("SELECT SUM(file_size) FROM upload_rate_limit WHERE user_id=1 AND reason='avatar' AND uploaded_at>'2030-01-01T00:00:00Z'", "idx_upload_rate_limit_user_reason_time"),
        ] {
            crate::native::assert_indexed_plan(f.pool(), sql, &[index]).await;
        }
        f.finish().await;
    }
}
