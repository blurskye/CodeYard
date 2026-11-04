//! Contribution authority on disposable PostgreSQL and SQLite databases: who may
//! open, comment on and review issues and pull requests. Setup failure fails the
//! test; nothing is skipped. Database-level contracts only.
//! Comments cannot be updated or deleted, so those are not covered; do not
//! substitute issue or PR editing to test them.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database::wait_until_blocked;
use crate::support::database::Database;
use crate::support::native_database as native;
use crate::support::organizations::applied;
use crate::support::production_ci as production;

use codeyard_db_repos::discussions::DiscussionEdit;

use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::Issue;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::Repository;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DENIED: &str = "discussion_authority_denied";
static NEXT_PR_BRANCH: AtomicU64 = AtomicU64::new(1);

impl Database {
    async fn new_repo(
        &self,
        owner: i64,
        org: Option<i64>,
        name: &str,
        private: bool,
    ) -> Repository {
        let repo = codeyard_db_repos::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            private,
            "/owned/not-executed.git",
        )
        .await
        .unwrap();
        if let Some(org) = org {
            self.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, repo.id]).await;
        }
        codeyard_db_repos::repositories::records::get_repository_by_id(self.pool(), repo.id)
            .await
            .unwrap()
            .unwrap()
    }
    async fn snapshot(&self, repo: i64, issue: i64, pr: i64) -> Value {
        let counters: Vec<(i64, i64)> = match self {
            Self::Postgres(f) => sqlx::query_as(
                "SELECT repo_id,next_number FROM repository_discussion_counters ORDER BY repo_id",
            )
            .fetch_all(&f.raw)
            .await
            .unwrap(),
            Self::Sqlite(f) => sqlx::query_as(
                "SELECT repo_id,next_number FROM repository_discussion_counters ORDER BY repo_id",
            )
            .fetch_all(&f.raw)
            .await
            .unwrap(),
        };
        json!({
            "repo": codeyard_db_repos::repositories::records::get_repository_by_id(self.pool(), repo).await.unwrap(),
            "issues": codeyard_db_repos::issues::list_repo_issues(self.pool(), repo, &Default::default(), 4096).await.unwrap().into_iter().map(|entry| entry.issue).collect::<Vec<_>>(),
            "prs": codeyard_db_repos::pull_requests::records::list_repo_pull_requests(self.pool(), repo, &Default::default(), 4096).await.unwrap().into_iter().map(|entry| entry.pull_request).collect::<Vec<_>>(),
            "comments": codeyard_db_repos::issues::list_issue_comments(self.pool(), issue, 100, 0).await.unwrap().into_iter().map(|entry| entry.comment).collect::<Vec<_>>(),
            "pr_comments": codeyard_db_repos::pull_requests::reviews::list_pull_request_comments(self.pool(), pr, 100, 0).await.unwrap().into_iter().map(|entry| entry.comment).collect::<Vec<_>>(),
            "reviews": codeyard_db_repos::pull_requests::reviews::list_pull_request_reviews(self.pool(), pr, 100, 0).await.unwrap().into_iter().map(|entry| entry.review).collect::<Vec<_>>(),
            "reviewers": codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(self.pool(), pr).await.unwrap().iter().map(|u| u.id).collect::<Vec<_>>(),
            "counters": counters,
        })
    }
    async fn corrupt_scope(&self, repo: i64, sql: &str) {
        // Deliberately malformed facts in an owned fixture only. Production
        // constraints and migrations are untouched, and normal SQLite checks resume.
        match self {
            Self::Postgres(f) => {
                sqlx::query("ALTER TABLE repositories DROP CONSTRAINT IF EXISTS repo_owner_check")
                    .execute(&f.raw)
                    .await
                    .unwrap();
                sqlx::query(sql).bind(repo).execute(&f.raw).await.unwrap();
            }
            Self::Sqlite(f) => {
                let mut c = f.raw.acquire().await.unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=ON")
                    .execute(&mut *c)
                    .await
                    .unwrap();
                sqlx::query(sql).bind(repo).execute(&mut *c).await.unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=OFF")
                    .execute(&mut *c)
                    .await
                    .unwrap();
            }
        }
    }
}
fn refusal<T: std::fmt::Debug>(result: anyhow::Result<T>) {
    assert_eq!(result.unwrap_err().to_string(), DENIED);
}
async fn new_pr(
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    actor: i64,
) -> anyhow::Result<PullRequest> {
    let branch = format!(
        "feature-{actor}-{}",
        NEXT_PR_BRANCH.fetch_add(1, Ordering::Relaxed)
    );
    codeyard_db_repos::pull_requests::records::create_pull_request(
        pool,
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            head_repo_id: None,
            repo_id: repo,
            author_id: actor,
            title: "Contribution",
            body: "Fixture",
            head_branch: &branch,
            head_sha: SHA,
            base_branch: "main",
            base_sha: SHA,
            is_draft: false,
            maintainer_can_modify: false,
            issue: None,
        },
    )
    .await
}
fn comment(
    pr: i64,
    actor: i64,
    parent: Option<i64>,
) -> codeyard_db_repos::pull_requests::reviews::NewPullRequestComment<'static> {
    codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
        pr_id: pr,
        author_id: actor,
        body: "Contribution",
        start_line: None,
        start_side: None,
        expected_revisions: None,
        path: None,
        line: None,
        side: None,
        commit_sha: None,
        in_reply_to_comment_id: parent,
    }
}
async fn contributions(d: &Database, repo: i64, actor: i64, issue: i64, pr: i64, allowed: bool) {
    let before = d.snapshot(repo, issue, pr).await;
    // A missing account cannot hold a session, so it presents one that does not exist.
    let session = if codeyard_db_identity::users::accounts::get_user_by_id(d.pool(), actor)
        .await
        .unwrap()
        .is_some()
    {
        crate::support::live_session(d.pool(), actor).await
    } else {
        "0".repeat(64)
    };
    let i =
        codeyard_db_repos::issues::create_issue(d.pool(), repo, actor, "Contribution", "Fixture")
            .await;
    let p = new_pr(d.pool(), repo, actor).await;
    let ic = codeyard_db_repos::issues::add_issue_comment(
        d.pool(),
        repo,
        issue,
        actor,
        "Contribution",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session),
    )
    .await;
    let pc = codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
        d.pool(),
        comment(pr, actor, None),
    )
    .await;
    if allowed {
        assert_eq!(i.unwrap().author_id, actor);
        assert_eq!(p.unwrap().author_id, actor);
        assert_eq!(ic.unwrap().author_id, actor);
        assert_eq!(pc.unwrap().author_id, actor);
    } else {
        refusal(i);
        refusal(p);
        refusal(ic);
        refusal(pc);
        assert_eq!(
            d.snapshot(repo, issue, pr).await,
            before,
            "denial changed rows/counters"
        );
    }
}
/// Any review, vote or comment, needs the same read authority as a comment, as on GitHub. Where
/// the actor may not comment, the review is refused and nothing is stored.
async fn review_refused(d: &Database, pr: i64, actor: i64) {
    for state in ["commented", "approved", "changes_requested"] {
        refusal(
            codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
                d.pool(),
                pr,
                actor,
                &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                    &crate::support::live_session(d.pool(), actor).await,
                ),
                state,
                Some("Denied"),
                Some(SHA),
            )
            .await,
        );
    }
}
async fn review_denied(
    d: &Database,
    repo: i64,
    actor: i64,
    issue: i64,
    pr: i64,
    review: i64,
    target: i64,
) {
    let before = d.snapshot(repo, issue, pr).await;
    // Dismissing and requesting reviewers need write authority. Reviews are checked by
    // review_refused, since a reader may vote.
    refusal(
        codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
            d.pool(),
            review,
            actor,
            Some("Denied"),
        )
        .await,
    );
    refusal(
        codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
            d.pool(),
            pr,
            actor,
            target,
        )
        .await,
    );
    refusal(
        codeyard_db_repos::pull_requests::reviews::remove_pull_request_reviewer(
            d.pool(),
            pr,
            actor,
            target,
        )
        .await,
    );
    assert_eq!(d.snapshot(repo, issue, pr).await, before);
}
/// Edits the title and body of `issue` and `pr`, and checks that both edits are `allowed` or both
/// refused.
async fn edits(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    issue: &Issue,
    pr: &PullRequest,
    allowed: bool,
) {
    let issue_edit = codeyard_db_repos::issues::update_issue(
        pool,
        issue.id,
        DiscussionEdit {
            actor_id: actor,
            expected_state: &issue.state,
            expected_updated: issue.updated_at,
            title: Some("Edited title"),
            body: Some("Edited body"),
            state: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(issue_edit.is_some(), allowed);
    let pr_edit = codeyard_db_repos::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        DiscussionEdit {
            actor_id: actor,
            expected_state: &pr.state,
            expected_updated: pr.updated_at,
            title: Some("Edited title"),
            body: Some("Edited body"),
            state: None,
        },
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(pr_edit.is_some(), allowed);
}
async fn edit_denied(pool: &codeyard_db_core::DbPool, actor: i64, issue: &Issue, pr: &PullRequest) {
    edits(pool, actor, issue, pr, false).await;
}

async fn contract(pg: bool) {
    let mut d = Database::new(pg).await;
    let owner = d.actor("contribution_owner").await;
    let admin = d.actor("contribution_admin").await;
    let member = d.actor("contribution_member").await;
    let reader = d.actor("contribution_reader").await;
    let other = d.actor("contribution_other").await;
    let writer = d.actor("contribution_writer").await;
    let site = d.actor("contribution_site").await;
    d.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            d.pool(),
            owner,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "contribution_org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for (actor, role) in [(admin, OrgRole::Admin), (member, OrgRole::Member)] {
        applied(
            codeyard_db_orgs::organizations::membership::add_organization_member(
                d.pool(),
                owner,
                org,
                actor,
                role,
            )
            .await
            .unwrap(),
        );
    }
    let public = d
        .new_repo(owner, Some(org), "contribution_public", false)
        .await;
    let private = d
        .new_repo(owner, Some(org), "contribution_private", true)
        .await;
    let personal = d.new_repo(owner, None, "contribution_personal", true).await;
    let foreign = d.new_repo(other, None, "contribution_foreign", true).await;
    let issue =
        codeyard_db_repos::issues::create_issue(d.pool(), private.id, owner, "Original", "Fixture")
            .await
            .unwrap();
    let pr = new_pr(d.pool(), private.id, owner).await.unwrap();
    let public_issue =
        codeyard_db_repos::issues::create_issue(d.pool(), public.id, owner, "Original", "Fixture")
            .await
            .unwrap();
    let public_pr = new_pr(d.pool(), public.id, owner).await.unwrap();
    let personal_issue = codeyard_db_repos::issues::create_issue(
        d.pool(),
        personal.id,
        owner,
        "Original",
        "Fixture",
    )
    .await
    .unwrap();
    let personal_pr = new_pr(d.pool(), personal.id, owner).await.unwrap();
    let foreign_issue =
        codeyard_db_repos::issues::create_issue(d.pool(), foreign.id, other, "Original", "Fixture")
            .await
            .unwrap();
    let foreign_pr = new_pr(d.pool(), foreign.id, other).await.unwrap();
    for repo in [private.id, public.id, personal.id] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            d.pool(),
            writer,
            repo,
            AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    let review = codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), writer).await,
        ),
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    let public_review = codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        public_pr.id,
        writer,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), writer).await,
        ),
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    for actor in [reader, member, site] {
        contributions(&d, public.id, actor, public_issue.id, public_pr.id, true).await;
        contributions(&d, private.id, actor, issue.id, pr.id, false).await;
        review_denied(
            &d,
            public.id,
            actor,
            public_issue.id,
            public_pr.id,
            public_review.id,
            writer,
        )
        .await;
    }
    // No site-admin org override, but personal compatibility remains intact.
    contributions(
        &d,
        personal.id,
        site,
        personal_issue.id,
        personal_pr.id,
        true,
    )
    .await;
    let personal_review = codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        personal_pr.id,
        writer,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), writer).await,
        ),
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
        d.pool(),
        personal_review.id,
        site,
        Some("Personal policy"),
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        reader,
        private.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        reader,
        personal.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    for (repo, i, p) in [
        (private.id, issue.id, pr.id),
        (personal.id, personal_issue.id, personal_pr.id),
    ] {
        contributions(&d, repo, reader, i, p, true).await;
    }
    review_denied(&d, private.id, reader, issue.id, pr.id, review.id, writer).await;
    // Foreign/private and absent targets have identical recognizable refusals at
    // the actual named DAO boundary, retaining the caller's explicit repository scope.
    contributions(
        &d,
        foreign.id,
        reader,
        foreign_issue.id,
        foreign_pr.id,
        false,
    )
    .await;
    refusal(
        codeyard_db_repos::issues::add_issue_comment(
            d.pool(),
            private.id,
            i64::MAX,
            reader,
            "Missing",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(d.pool(), reader).await,
            ),
        )
        .await,
    );
    refusal(
        codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
            d.pool(),
            comment(i64::MAX, reader, None),
        )
        .await,
    );
    contributions(&d, private.id, i64::MAX, issue.id, pr.id, false).await;
    refusal(new_pr(d.pool(), i64::MAX, reader).await);
    refusal(
        codeyard_db_repos::issues::create_issue(d.pool(), i64::MAX, reader, "Missing", "Missing")
            .await,
    );
    let parent = codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
        d.pool(),
        comment(foreign_pr.id, other, None),
    )
    .await
    .unwrap();
    // The intentional parent insertion also advances its PR revision. Snapshot
    // after that accepted mutation, then compare ALL foreign data after denial.
    let unrelated = d
        .snapshot(foreign.id, foreign_issue.id, foreign_pr.id)
        .await;
    let before = d.snapshot(private.id, issue.id, pr.id).await;
    refusal(
        codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
            d.pool(),
            comment(pr.id, reader, Some(parent.id)),
        )
        .await,
    );
    assert_eq!(d.snapshot(private.id, issue.id, pr.id).await, before);
    assert_eq!(
        d.snapshot(foreign.id, foreign_issue.id, foreign_pr.id)
            .await,
        unrelated
    );

    // Write reviewers retain power, never Read/Triage outsiders; own approval and
    // closed-PR review submission are rechecked under the admitted parent lock.
    assert!(
        codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
            d.pool(),
            pr.id,
            owner,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(d.pool(), owner).await
            ),
            "approved",
            None,
            Some(SHA)
        )
        .await
        .is_err()
    );
    let before = d.snapshot(private.id, issue.id, pr.id).await;
    for target in [reader, other, i64::MAX] {
        refusal(
            codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
                d.pool(),
                pr.id,
                owner,
                target,
            )
            .await,
        );
        assert_eq!(d.snapshot(private.id, issue.id, pr.id).await, before);
    }
    codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
        d.pool(),
        pr.id,
        owner,
        writer,
    )
    .await
    .unwrap();
    let requested =
        codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(d.pool(), pr.id)
            .await
            .unwrap();
    assert!(
        requested.iter().any(|user| user.id == writer),
        "review request must exist before the review clears it"
    );
    codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), writer).await,
        ),
        "commented",
        Some("Reviewed"),
        Some(SHA),
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(d.pool(), pr.id)
            .await
            .unwrap()
            .is_empty()
    );
    for actor in [writer, reader] {
        refusal(
            codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
                d.pool(),
                review.id,
                actor,
                None,
            )
            .await,
        );
    }
    codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
        d.pool(),
        review.id,
        admin,
        Some("Org Admin"),
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        writer,
        private.id,
        AccessLevel::Triage,
    )
    .await
    .unwrap();
    contributions(&d, private.id, writer, issue.id, pr.id, true).await;
    review_denied(&d, private.id, writer, issue.id, pr.id, review.id, reader).await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        writer,
        private.id,
        AccessLevel::Write,
    )
    .await
    .unwrap();

    // Caller-supplied CAS survives comment/review touches. Never reload and
    // substitute the revision, including when current authority is sufficient.
    let stale_issue = codeyard_db_repos::issues::get_issue_by_id(d.pool(), issue.id)
        .await
        .unwrap()
        .unwrap();
    let stale_pr =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(d.pool(), pr.id)
            .await
            .unwrap()
            .unwrap();
    codeyard_db_repos::issues::add_issue_comment(
        d.pool(),
        issue.repo_id,
        issue.id,
        reader,
        "Invalidate revision",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), reader).await,
        ),
    )
    .await
    .unwrap();
    codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(d.pool(), writer).await,
        ),
        "commented",
        Some("Reviewed"),
        Some(SHA),
    )
    .await
    .unwrap();
    let before = d.snapshot(private.id, issue.id, pr.id).await;
    edit_denied(d.pool(), owner, &stale_issue, &stale_pr).await;
    let current_issue = codeyard_db_repos::issues::get_issue_by_id(d.pool(), issue.id)
        .await
        .unwrap()
        .unwrap();
    let current_pr =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(d.pool(), pr.id)
            .await
            .unwrap()
            .unwrap();
    // As on GitHub, a reader may edit the issue and the pull request they opened, and no
    // others.
    let reader_issue = codeyard_db_repos::issues::create_issue(
        d.pool(),
        private.id,
        reader,
        "Read author",
        "Fixture",
    )
    .await
    .unwrap();
    let reader_pr = new_pr(d.pool(), private.id, reader).await.unwrap();
    edits(d.pool(), reader, &reader_issue, &reader_pr, true).await;
    let before_edits = d.snapshot(private.id, issue.id, pr.id).await;
    edit_denied(d.pool(), reader, &current_issue, &current_pr).await;
    let imprecise_issue = Issue {
        updated_at: current_issue.updated_at + chrono::Duration::nanoseconds(1),
        ..current_issue.clone()
    };
    let imprecise_pr = PullRequest {
        updated_at: current_pr.updated_at + chrono::Duration::nanoseconds(1),
        ..current_pr.clone()
    };
    edit_denied(d.pool(), owner, &imprecise_issue, &imprecise_pr).await;
    let wrong_issue = Issue {
        state: "closed".to_string(),
        ..current_issue.clone()
    };
    let wrong_pr = PullRequest {
        state: "closed".to_string(),
        ..current_pr.clone()
    };
    edit_denied(d.pool(), owner, &wrong_issue, &wrong_pr).await;
    assert_eq!(d.snapshot(private.id, issue.id, pr.id).await, before_edits);
    let after = d.snapshot(private.id, issue.id, pr.id).await;
    assert_eq!(before["comments"], after["comments"]);
    assert_eq!(before["pr_comments"], after["pr_comments"]);
    assert_eq!(before["reviews"], after["reviews"]);
    let closed = codeyard_db_repos::pull_requests::records::update_pull_request(
        d.pool(),
        pr.id,
        DiscussionEdit {
            actor_id: owner,
            expected_state: &current_pr.state,
            expected_updated: current_pr.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
        Default::default(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(closed.state, "closed");
    assert!(
        codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
            d.pool(),
            pr.id,
            writer,
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(d.pool(), writer).await
            ),
            "approved",
            None,
            Some(SHA)
        )
        .await
        .is_err()
    );
    codeyard_db_repos::pull_requests::records::update_pull_request(
        d.pool(),
        pr.id,
        DiscussionEdit {
            actor_id: owner,
            expected_state: &closed.state,
            expected_updated: closed.updated_at,
            title: None,
            body: None,
            state: Some("open"),
        },
        Default::default(),
    )
    .await
    .unwrap()
    .unwrap();

    // Cross-tenant team facts never become grants for the current repo. Same-org
    // Read team contributions work, then current membership revocation wins.
    let team = applied(
        codeyard_db_orgs::organizations::teams::create_organization_team(
            d.pool(),
            owner,
            org,
            &codeyard_db_orgs::organizations::teams::NewTeam::new(
                "contribution_team",
                codeyard_db_orgs::organizations::teams::TeamProfile::new(
                    "Contribution",
                    None,
                    codeyard_db_orgs::organizations::teams::TeamPrivacy::Secret,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        codeyard_db_orgs::organizations::teams::add_organization_team_member(
            d.pool(),
            owner,
            org,
            team,
            member,
            codeyard_db_orgs::organizations::teams::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::teams::grant_organization_team_repository(
            d.pool(),
            owner,
            org,
            team,
            private.id,
            codeyard_db_orgs::organizations::teams::TeamRepositoryPermission::new(1).unwrap(),
        )
        .await
        .unwrap(),
    );
    contributions(&d, private.id, member, issue.id, pr.id, true).await;
    review_denied(&d, private.id, member, issue.id, pr.id, review.id, writer).await;
    applied(
        codeyard_db_orgs::organizations::teams::remove_organization_team_member(
            d.pool(),
            owner,
            org,
            team,
            member,
        )
        .await
        .unwrap(),
    );
    contributions(&d, private.id, member, issue.id, pr.id, false).await;
    applied(
        codeyard_db_orgs::organizations::teams::add_organization_team_member(
            d.pool(),
            owner,
            org,
            team,
            member,
            codeyard_db_orgs::organizations::teams::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    for sql in [
        "UPDATE team_repo_access SET permission=6 WHERE team_id=$1 AND repo_id=$2",
        "UPDATE team_repo_access SET permission=0 WHERE team_id=$1 AND repo_id=$2",
    ] {
        d.execute(sql, &[team, private.id]).await;
        contributions(&d, private.id, member, issue.id, pr.id, false).await;
    }
    d.execute(
        "UPDATE team_repo_access SET permission=1 WHERE team_id=$1 AND repo_id=$2",
        &[team, private.id],
    )
    .await;
    d.execute(
        "UPDATE team_memberships SET role='invalid' WHERE team_id=$1 AND user_id=$2",
        &[team, member],
    )
    .await;
    contributions(&d, private.id, member, issue.id, pr.id, false).await;
    d.execute(
        "UPDATE team_memberships SET role='member' WHERE team_id=$1 AND user_id=$2",
        &[team, member],
    )
    .await;
    let foreign_org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            d.pool(),
            other,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "contribution_foreign_org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    d.execute(
        "UPDATE teams SET org_id=$1 WHERE id=$2",
        &[foreign_org, team],
    )
    .await;
    contributions(&d, private.id, member, issue.id, pr.id, false).await;
    // Current grant/role revocation wins over any previously observed permission.
    codeyard_db_repos::repositories::access::revoke_repo_access(d.pool(), reader, private.id)
        .await
        .unwrap();
    contributions(&d, private.id, reader, issue.id, pr.id, false).await;
    applied(
        codeyard_db_orgs::organizations::membership::update_organization_member_role(
            d.pool(),
            owner,
            org,
            admin,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    contributions(&d, private.id, admin, issue.id, pr.id, false).await;
    review_denied(&d, private.id, admin, issue.id, pr.id, review.id, writer).await;
    review_refused(&d, pr.id, admin).await;
    applied(
        codeyard_db_orgs::organizations::membership::update_organization_member_role(
            d.pool(),
            owner,
            org,
            admin,
            OrgRole::Admin,
        )
        .await
        .unwrap(),
    );
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        reader,
        private.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    for flag in ["is_banned", "must_change_password"] {
        d.execute(
            &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
            &[reader],
        )
        .await;
        contributions(&d, public.id, reader, public_issue.id, public_pr.id, false).await;
        contributions(&d, private.id, reader, issue.id, pr.id, false).await;
        d.execute(
            &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
            &[reader],
        )
        .await;
        d.execute(
            &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
            &[writer],
        )
        .await;
        review_denied(&d, private.id, writer, issue.id, pr.id, review.id, reader).await;
        review_refused(&d, pr.id, writer).await;
        d.execute(
            &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
            &[writer],
        )
        .await;
        d.execute(
            &format!("UPDATE users SET {flag}=TRUE WHERE id=$1"),
            &[owner],
        )
        .await;
        for actor in [reader, writer, admin, member, site] {
            assert_eq!(
                codeyard_db_repos::repositories::access::get_user_repo_access(
                    d.pool(),
                    actor,
                    public.id
                )
                .await
                .unwrap(),
                AccessLevel::Read
            );
            contributions(&d, public.id, actor, public_issue.id, public_pr.id, false).await;
            contributions(&d, private.id, actor, issue.id, pr.id, false).await;
            review_denied(&d, private.id, actor, issue.id, pr.id, review.id, reader).await;
            review_refused(&d, pr.id, actor).await;
        }
        d.execute(
            &format!("UPDATE users SET {flag}=FALSE WHERE id=$1"),
            &[owner],
        )
        .await;
    }
    // An archived repository takes no new contributions.
    d.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[private.id],
    )
    .await;
    contributions(&d, private.id, owner, issue.id, pr.id, false).await;
    review_denied(&d, private.id, writer, issue.id, pr.id, review.id, reader).await;
    review_refused(&d, pr.id, writer).await;
    d.execute(
        "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
        &[private.id],
    )
    .await;
    // A deleted repository takes no contributions either.
    let doomed = d.new_repo(owner, None, "contribution_deleted", true).await;
    let doomed_issue =
        codeyard_db_repos::issues::create_issue(d.pool(), doomed.id, owner, "Original", "Fixture")
            .await
            .unwrap();
    let doomed_pr = new_pr(d.pool(), doomed.id, owner).await.unwrap();
    d.execute("DELETE FROM repositories WHERE id=$1", &[doomed.id])
        .await;
    for actor in [owner, site] {
        contributions(&d, doomed.id, actor, doomed_issue.id, doomed_pr.id, false).await;
    }
    // Malformed direct Owner/out-of-range grants and unknown tenant roles do not
    // confer access, even if callers cached a formerly valid row.
    for level in [0, 6, 7, -1] {
        d.execute(
            &format!("UPDATE repo_access SET access_level={level} WHERE user_id=$1 AND repo_id=$2"),
            &[reader, private.id],
        )
        .await;
        contributions(&d, private.id, reader, issue.id, pr.id, false).await;
    }
    d.execute(
        "UPDATE org_memberships SET role='invalid' WHERE org_id=$1 AND user_id=$2",
        &[org, admin],
    )
    .await;
    contributions(&d, private.id, admin, issue.id, pr.id, false).await;
    review_denied(&d, private.id, admin, issue.id, pr.id, review.id, writer).await;
    review_refused(&d, pr.id, admin).await;
    d.execute(
        "UPDATE org_memberships SET role='invalid' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    contributions(&d, public.id, other, public_issue.id, public_pr.id, false).await;
    d.execute(
        "UPDATE org_memberships SET role='owner' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    // NULL scope is never adopted as personal or tenant ownership, nor can a
    // platform administrator repair it implicitly while submitting a comment.
    for sql in [
        "UPDATE repositories SET owner_kind='invalid' WHERE id=$1",
        "UPDATE repositories SET owner_kind='org',owner_org_id=NULL,owner_user_id=NULL WHERE id=$1",
        "UPDATE repositories SET owner_kind='user',owner_user_id=NULL WHERE id=$1",
        "UPDATE repositories SET owner_kind='org',owner_user_id=owner_org_id WHERE id=$1",
    ] {
        d.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, public.id]).await;
        d.corrupt_scope(public.id, sql).await;
        contributions(&d, public.id, site, public_issue.id, public_pr.id, false).await;
    }
    let unrelated_after = d
        .snapshot(foreign.id, foreign_issue.id, foreign_pr.id)
        .await;
    for key in [
        "repo",
        "issues",
        "prs",
        "comments",
        "pr_comments",
        "reviews",
        "reviewers",
    ] {
        assert_eq!(
            unrelated_after[key], unrelated[key],
            "unrelated {key} changed"
        );
    }
    d.finish().await;
}

async fn file_comment(pool: &codeyard_db_core::DbPool, pr: i64, author: i64) -> i64 {
    codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
        pool,
        codeyard_db_repos::pull_requests::reviews::NewPullRequestComment {
            pr_id: pr,
            author_id: author,
            body: "Conversation",
            start_line: None,
            start_side: None,
            expected_revisions: None,
            path: Some("src/lib.rs"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(SHA),
            in_reply_to_comment_id: None,
        },
    )
    .await
    .unwrap()
    .id
}
async fn resolve(
    pool: &codeyard_db_core::DbPool,
    pr: i64,
    comment: i64,
    resolved: bool,
    actor: i64,
) -> anyhow::Result<Option<codeyard_db_core::models::PullRequestComment>> {
    codeyard_db_repos::pull_requests::reviews::set_pull_request_comment_resolved(
        pool, pr, comment, resolved, actor,
    )
    .await
}
async fn still_open(pool: &codeyard_db_core::DbPool, comment: i64) -> bool {
    codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(pool, comment)
        .await
        .unwrap()
        .unwrap()
        .resolved_at
        .is_none()
}

/// Resolving a conversation is checked in the transaction that moves it: the live actor, an
/// unarchived repository and a usable owner, then write access or the author's read access.
async fn comment_resolution_contract(pg: bool) {
    let mut d = Database::new(pg).await;
    let owner = d.actor("resolve_owner").await;
    let author = d.actor("resolve_author").await;
    let writer = d.actor("resolve_writer").await;
    let revoked = d.actor("resolve_revoked").await;
    let personal = d.new_repo(owner, None, "resolve_personal", true).await;
    for (actor, level) in [
        (author, AccessLevel::Read),
        (writer, AccessLevel::Write),
        (revoked, AccessLevel::Write),
    ] {
        codeyard_db_repos::repositories::access::grant_repo_access(
            d.pool(),
            actor,
            personal.id,
            level,
        )
        .await
        .unwrap();
    }
    codeyard_db_repos::repositories::access::revoke_repo_access(d.pool(), revoked, personal.id)
        .await
        .unwrap();
    let pr = new_pr(d.pool(), personal.id, author).await.unwrap();
    let thread = file_comment(d.pool(), pr.id, author).await;

    // The author who can read the repository, and a writer, may resolve and reopen it.
    let resolved = resolve(d.pool(), pr.id, thread, true, author)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.resolved_by_id, Some(author));
    let reopened = resolve(d.pool(), pr.id, thread, false, writer)
        .await
        .unwrap()
        .unwrap();
    assert!(reopened.resolved_at.is_none());

    // Every refusal leaves the conversation as it was.
    refusal(resolve(d.pool(), pr.id, thread, true, revoked).await);
    d.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[writer])
        .await;
    refusal(resolve(d.pool(), pr.id, thread, true, writer).await);
    d.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[writer])
        .await;
    d.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[personal.id],
    )
    .await;
    refusal(resolve(d.pool(), pr.id, thread, true, author).await);
    refusal(resolve(d.pool(), pr.id, thread, true, writer).await);
    d.execute(
        "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
        &[personal.id],
    )
    .await;
    assert!(still_open(d.pool(), thread).await);

    // An author keeps the right to resolve only while the owning organization has a usable owner.
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            d.pool(),
            owner,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "resolve_org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let shared = d.new_repo(owner, Some(org), "resolve_shared", true).await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        d.pool(),
        author,
        shared.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let shared_pr = new_pr(d.pool(), shared.id, author).await.unwrap();
    let shared_thread = file_comment(d.pool(), shared_pr.id, author).await;
    resolve(d.pool(), shared_pr.id, shared_thread, true, author)
        .await
        .unwrap()
        .unwrap();
    d.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[owner])
        .await;
    refusal(resolve(d.pool(), shared_pr.id, shared_thread, false, author).await);
    assert!(!still_open(d.pool(), shared_thread).await);
    // Finish the fixture on this runtime. Without it, the drop backstop runs cleanup on a thread
    // that blocks this runtime, so connections still queued for return here are never released.
    d.finish().await;
}

#[tokio::test]
async fn postgres_conversation_resolution_is_authorized_in_the_transaction() {
    comment_resolution_contract(true).await;
}
#[tokio::test]
async fn sqlite_conversation_resolution_is_authorized_in_the_transaction() {
    comment_resolution_contract(false).await;
}

#[tokio::test]
async fn postgres_organization_contributions() {
    contract(true).await;
}
#[tokio::test]
async fn sqlite_organization_contributions() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_contributions_recheck_after_grant_revocation_before_parent_locks() {
    let mut f = production::Fixture::new().await;
    let actor = codeyard_db_identity::users::accounts::create_user(
        &f.pool,
        "contribution_wait_reader",
        None,
    )
    .await
    .unwrap()
    .id;
    let target = codeyard_db_identity::users::accounts::create_user(
        &f.pool,
        "contribution_wait_target",
        None,
    )
    .await
    .unwrap()
    .id;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            &f.pool,
            f.owner.id,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "contribution_wait_org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    sqlx::query(
        "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
    )
    .bind(org)
    .bind(f.repo.id)
    .execute(&f.raw)
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        &f.pool,
        actor,
        f.repo.id,
        AccessLevel::Admin,
    )
    .await
    .unwrap();
    let issue =
        codeyard_db_repos::issues::create_issue(&f.pool, f.repo.id, f.owner.id, "Wait", "Fixture")
            .await
            .unwrap();
    let pr = new_pr(&f.pool, f.repo.id, f.owner.id).await.unwrap();
    let review = codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
        &f.pool,
        pr.id,
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(&f.pool, actor).await,
        ),
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    for operation in 0..8 {
        codeyard_db_repos::repositories::access::grant_repo_access(
            &f.pool,
            actor,
            f.repo.id,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
        let mut revoke = f.raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoke)
            .await
            .unwrap();
        // An ordinary projection can still see this uncommitted deletion. The
        // effectful authority path must wait on the grant row and then recheck.
        sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
            .bind(f.repo.id)
            .bind(actor)
            .execute(&mut *revoke)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let (repo_id, issue_id, pr_id, review_id) = (f.repo.id, issue.id, pr.id, review.id);
        let task = tokio::spawn(async move {
            match operation {
                0 => codeyard_db_repos::issues::create_issue(
                    &pool, repo_id, actor, "Denied", "Denied",
                )
                .await
                .map(|_| ()),
                1 => new_pr(&pool, repo_id, actor).await.map(|_| ()),
                2 => codeyard_db_repos::issues::add_issue_comment(
                    &pool,
                    repo_id,
                    issue_id,
                    actor,
                    "Denied",
                    &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                        &crate::support::live_session(&pool, actor).await,
                    ),
                )
                .await
                .map(|_| ()),
                3 => codeyard_db_repos::pull_requests::reviews::add_pull_request_comment(
                    &pool,
                    comment(pr_id, actor, None),
                )
                .await
                .map(|_| ()),
                4 => codeyard_db_repos::pull_requests::reviews::add_pull_request_review(
                    &pool,
                    pr_id,
                    actor,
                    &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                        &crate::support::live_session(&pool, actor).await,
                    ),
                    "approved",
                    None,
                    Some(SHA),
                )
                .await
                .map(|_| ()),
                5 => codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
                    &pool, pr_id, actor, target,
                )
                .await
                .map(|_| ()),
                6 => codeyard_db_repos::pull_requests::reviews::remove_pull_request_reviewer(
                    &pool, pr_id, actor, target,
                )
                .await
                .map(|_| ()),
                _ => codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
                    &pool, review_id, actor, None,
                )
                .await
                .map(|_| ()),
            }
        });
        wait_until_blocked(&f.raw, pid, 1).await;
        sqlx::query("SET LOCAL lock_timeout='1s'")
            .execute(&mut *revoke)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM issues WHERE id=$1 FOR UPDATE")
            .bind(issue.id)
            .execute(&mut *revoke)
            .await
            .expect("waiter must not own issue");
        sqlx::query("SELECT id FROM pull_requests WHERE id=$1 FOR UPDATE")
            .bind(pr.id)
            .execute(&mut *revoke)
            .await
            .expect("waiter must not own PR");
        revoke.commit().await.unwrap();
        refusal(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // A comment accepted before a later revocation stays, but a saved author ID
    // cannot authorize a replay.
    codeyard_db_repos::repositories::access::grant_repo_access(
        &f.pool,
        actor,
        f.repo.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let accepted = codeyard_db_repos::issues::add_issue_comment(
        &f.pool,
        issue.repo_id,
        issue.id,
        actor,
        "Accepted snapshot",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(&f.pool, actor).await,
        ),
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::revoke_repo_access(&f.pool, actor, f.repo.id)
        .await
        .unwrap();
    refusal(
        codeyard_db_repos::issues::add_issue_comment(
            &f.pool,
            issue.repo_id,
            issue.id,
            actor,
            "Replay",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(&f.pool, actor).await,
            ),
        )
        .await,
    );
    assert!(
        codeyard_db_repos::issues::list_issue_comments(&f.pool, issue.id, 100, 0)
            .await
            .unwrap()
            .iter()
            .any(|c| c.comment.id == accepted.id && c.comment.body == "Accepted snapshot")
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_contribution_keeps_its_grant_until_the_effect_commits() {
    let mut f = production::Fixture::new().await;
    let actor = codeyard_db_identity::users::accounts::create_user(
        &f.pool,
        "contribution_commit_reader",
        None,
    )
    .await
    .unwrap()
    .id;
    codeyard_db_repos::repositories::access::grant_repo_access(
        &f.pool,
        actor,
        f.repo.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let issue = codeyard_db_repos::issues::create_issue(
        &f.pool,
        f.repo.id,
        f.owner.id,
        "Commit order",
        "Fixture",
    )
    .await
    .unwrap();

    let mut parent = f.raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *parent)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM issues WHERE id=$1 FOR UPDATE")
        .bind(issue.id)
        .fetch_one(&mut *parent)
        .await
        .unwrap();
    let pool = f.pool.clone();
    let (repo_id, issue_id) = (issue.repo_id, issue.id);
    let effect = tokio::spawn(async move {
        codeyard_db_repos::issues::add_issue_comment(
            &pool,
            repo_id,
            issue_id,
            actor,
            "Accepted before revocation",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(&pool, actor).await,
            ),
        )
        .await
    });
    wait_until_blocked(&f.raw, blocker, 1).await;

    let pool = f.pool.clone();
    let repo = f.repo.id;
    let revocation = tokio::spawn(async move {
        codeyard_db_repos::repositories::access::revoke_repo_access(&pool, actor, repo).await
    });
    wait_until_blocked(&f.raw, blocker, 2).await;
    assert!(!revocation.is_finished());
    parent.commit().await.unwrap();

    let comment = effect.await.unwrap().unwrap();
    assert!(revocation.await.unwrap().unwrap());
    assert_eq!(comment.body, "Accepted before revocation");
    refusal(
        codeyard_db_repos::issues::add_issue_comment(
            &f.pool,
            issue.repo_id,
            issue.id,
            actor,
            "Denied after",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(&f.pool, actor).await,
            ),
        )
        .await,
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_team_contributions_wait_for_each_revoked_authority_row() {
    let mut f = production::Fixture::new().await;
    let actor =
        codeyard_db_identity::users::accounts::create_user(&f.pool, "team_wait_reader", None)
            .await
            .unwrap()
            .id;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            &f.pool,
            f.owner.id,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "team_wait_org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    sqlx::query(
        "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
    )
    .bind(org)
    .bind(f.repo.id)
    .execute(&f.raw)
    .await
    .unwrap();
    let team: i64 = sqlx::query_scalar(
        "INSERT INTO teams(org_id,slug,name,privacy) VALUES($1,'team-wait','Team wait','secret') RETURNING id",
    )
    .bind(org)
    .fetch_one(&f.raw)
    .await
    .unwrap();
    let issue =
        codeyard_db_repos::issues::create_issue(&f.pool, f.repo.id, f.owner.id, "Wait", "Fixture")
            .await
            .unwrap();

    for revoked_row in 0..3 {
        sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member') ON CONFLICT(org_id,user_id) DO UPDATE SET role='member'")
            .bind(org).bind(actor).execute(&f.raw).await.unwrap();
        sqlx::query("INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,'member') ON CONFLICT(team_id,user_id) DO UPDATE SET role='member'")
            .bind(team).bind(actor).execute(&f.raw).await.unwrap();
        sqlx::query("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,1) ON CONFLICT(team_id,repo_id) DO UPDATE SET permission=1")
            .bind(team).bind(f.repo.id).execute(&f.raw).await.unwrap();

        let mut revoke = f.raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoke)
            .await
            .unwrap();
        match revoked_row {
            0 => {
                sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
                    .bind(org)
                    .bind(actor)
                    .execute(&mut *revoke)
                    .await
                    .unwrap();
            }
            1 => {
                sqlx::query("DELETE FROM team_memberships WHERE team_id=$1 AND user_id=$2")
                    .bind(team)
                    .bind(actor)
                    .execute(&mut *revoke)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query("DELETE FROM team_repo_access WHERE team_id=$1 AND repo_id=$2")
                    .bind(team)
                    .bind(f.repo.id)
                    .execute(&mut *revoke)
                    .await
                    .unwrap();
            }
        }
        let pool = f.pool.clone();
        let (repo_id, issue_id) = (issue.repo_id, issue.id);
        let task = tokio::spawn(async move {
            codeyard_db_repos::issues::add_issue_comment(
                &pool,
                repo_id,
                issue_id,
                actor,
                "Denied",
                &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                    &crate::support::live_session(&pool, actor).await,
                ),
            )
            .await
        });
        wait_until_blocked(&f.raw, pid, 1).await;
        revoke.commit().await.unwrap();
        refusal(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(
        codeyard_db_repos::issues::list_issue_comments(&f.pool, issue.id, 100, 0)
            .await
            .unwrap()
            .is_empty()
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_contributions_use_reserved_writer_snapshot() {
    let f = native::NativeDatabase::new().await;
    let owner = codeyard_db_identity::users::accounts::create_user(
        &f.pool,
        "sqlite_contribution_owner",
        None,
    )
    .await
    .unwrap()
    .id;
    let actor = codeyard_db_identity::users::accounts::create_user(
        &f.pool,
        "sqlite_contribution_reader",
        None,
    )
    .await
    .unwrap()
    .id;
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &f.pool,
        owner,
        "sqlite_contribution",
        None,
        true,
        "/owned/not-executed.git",
    )
    .await
    .unwrap();
    let issue =
        codeyard_db_repos::issues::create_issue(&f.pool, repo.id, owner, "Reserved", "Fixture")
            .await
            .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        &f.pool,
        actor,
        repo.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let accepted = codeyard_db_repos::issues::add_issue_comment(
        &f.pool,
        issue.repo_id,
        issue.id,
        actor,
        "Accepted snapshot",
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(&f.pool, actor).await,
        ),
    )
    .await
    .unwrap();
    // Establish committed revocation through an independent writer reservation.
    // No sleep/unfinished-task assertion pretends to prove a SQLite lock wait.
    let mut revoke = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("DELETE FROM repo_access WHERE repo_id=?1 AND user_id=?2")
        .bind(repo.id)
        .bind(actor)
        .execute(&mut *revoke)
        .await
        .unwrap();
    revoke.commit().await.unwrap();
    let second = f.second_handle().await;
    refusal(
        codeyard_db_repos::issues::add_issue_comment(
            &second,
            issue.repo_id,
            issue.id,
            actor,
            "Revoked snapshot",
            &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
                &crate::support::live_session(&second, actor).await,
            ),
        )
        .await,
    );
    let comments = codeyard_db_repos::issues::list_issue_comments(&second, issue.id, 100, 0)
        .await
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].comment.id, accepted.id);
    second.close().await;
    f.close().await;
}
