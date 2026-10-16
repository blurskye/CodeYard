//! Contribution authority on disposable PostgreSQL and SQLite databases: who may
//! open, comment on and review issues and pull requests. Setup failure fails the
//! test; nothing is skipped. Database-level contracts only.
//! Comments cannot be updated or deleted, so those are not covered; do not
//! substitute issue or PR editing to test them.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

use codeyard::backend::db::{
    self,
    discussions::DiscussionEdit,
    models::{AccessLevel, Issue, OrgRole, PullRequest, Repository},
    organizations::{self as orgs, OrganizationOutcome},
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DENIED: &str = "discussion_authority_denied";
static NEXT_PR_BRANCH: AtomicU64 = AtomicU64::new(1);

enum Database {
    Postgres(Box<production::Fixture>),
    Sqlite(native::NativeDatabase),
}
impl Database {
    async fn new(pg: bool) -> Self {
        if pg {
            Self::Postgres(Box::new(production::Fixture::new().await))
        } else {
            Self::Sqlite(native::NativeDatabase::new().await)
        }
    }
    fn pool(&self) -> &db::DbPool {
        match self {
            Self::Postgres(f) => &f.pool,
            Self::Sqlite(f) => &f.pool,
        }
    }
    async fn execute(&self, sql: &str, ids: &[i64]) {
        match self {
            Self::Postgres(f) => {
                let mut q = sqlx::query(sql);
                for id in ids {
                    q = q.bind(*id);
                }
                q.execute(&f.raw).await.expect("owned PG mutation");
            }
            Self::Sqlite(f) => {
                let mut q = sqlx::query(sql);
                for id in ids {
                    q = q.bind(*id);
                }
                q.execute(&f.raw).await.expect("owned SQLite mutation");
            }
        }
    }
    async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    async fn repo(&self, owner: i64, org: Option<i64>, name: &str, private: bool) -> Repository {
        let repo = db::repositories::records::create_repository(
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
        db::repositories::records::get_repository_by_id(self.pool(), repo.id)
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
            "repo": db::repositories::records::get_repository_by_id(self.pool(), repo).await.unwrap(),
            "issues": db::issues::list_repo_issues(self.pool(), repo, &Default::default(), 4096).await.unwrap().into_iter().map(|entry| entry.issue).collect::<Vec<_>>(),
            "prs": db::pull_requests::records::list_repo_pull_requests(self.pool(), repo, &Default::default(), 4096).await.unwrap().into_iter().map(|entry| entry.pull_request).collect::<Vec<_>>(),
            "comments": db::issues::list_issue_comments(self.pool(), issue, 100, 0).await.unwrap().into_iter().map(|entry| entry.comment).collect::<Vec<_>>(),
            "pr_comments": db::pull_requests::reviews::list_pull_request_comments(self.pool(), pr, 100, 0).await.unwrap().into_iter().map(|entry| entry.comment).collect::<Vec<_>>(),
            "reviews": db::pull_requests::reviews::list_pull_request_reviews(self.pool(), pr, 100, 0).await.unwrap().into_iter().map(|entry| entry.review).collect::<Vec<_>>(),
            "reviewers": db::pull_requests::reviews::list_pull_request_reviewers(self.pool(), pr).await.unwrap().iter().map(|u| u.id).collect::<Vec<_>>(),
            "counters": counters,
        })
    }
    async fn corrupt_scope(&self, repo: i64, sql: &str) {
        // Deliberately malformed legacy facts in an owned fixture only. Production
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
    async fn close(&mut self) {
        match self {
            Self::Postgres(f) => f.finish().await,
            Self::Sqlite(f) => f.close().await,
        }
    }
}
fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected Applied: {other:?}"),
    }
}
fn refusal<T: std::fmt::Debug>(result: anyhow::Result<T>) {
    assert_eq!(result.unwrap_err().to_string(), DENIED);
}
async fn new_pr(pool: &db::DbPool, repo: i64, actor: i64) -> anyhow::Result<PullRequest> {
    let branch = format!(
        "feature-{actor}-{}",
        NEXT_PR_BRANCH.fetch_add(1, Ordering::Relaxed)
    );
    db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
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
        },
    )
    .await
}
fn comment(
    pr: i64,
    actor: i64,
    parent: Option<i64>,
) -> db::pull_requests::reviews::NewPullRequestComment<'static> {
    db::pull_requests::reviews::NewPullRequestComment {
        pr_id: pr,
        author_id: actor,
        body: "Contribution",
        path: None,
        line: None,
        side: None,
        commit_sha: None,
        in_reply_to_comment_id: parent,
    }
}
async fn contributions(d: &Database, repo: i64, actor: i64, issue: i64, pr: i64, allowed: bool) {
    let before = d.snapshot(repo, issue, pr).await;
    let i = db::issues::create_issue(d.pool(), repo, actor, "Contribution", "Fixture").await;
    let p = new_pr(d.pool(), repo, actor).await;
    let ic = db::issues::add_issue_comment(d.pool(), issue, actor, "Contribution").await;
    let pc =
        db::pull_requests::reviews::add_pull_request_comment(d.pool(), comment(pr, actor, None))
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
/// A plain comment review needs the same read authority as a comment. Where the actor may not
/// comment, the review is refused and nothing is stored.
async fn comment_review_refused(d: &Database, pr: i64, actor: i64) {
    refusal(
        db::pull_requests::reviews::add_pull_request_review(
            d.pool(),
            pr,
            actor,
            "commented",
            Some("Denied"),
            Some(SHA),
        )
        .await,
    );
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
    // Votes need write authority. A plain comment review is checked by comment_review_refused.
    for state in ["approved", "changes_requested"] {
        refusal(
            db::pull_requests::reviews::add_pull_request_review(
                d.pool(),
                pr,
                actor,
                state,
                Some("Denied"),
                Some(SHA),
            )
            .await,
        );
    }
    refusal(
        db::pull_requests::reviews::dismiss_pull_request_review(
            d.pool(),
            review,
            actor,
            Some("Denied"),
        )
        .await,
    );
    refusal(
        db::pull_requests::reviews::request_pull_request_reviewer(d.pool(), pr, actor, target)
            .await,
    );
    refusal(
        db::pull_requests::reviews::remove_pull_request_reviewer(d.pool(), pr, actor, target).await,
    );
    assert_eq!(d.snapshot(repo, issue, pr).await, before);
}
async fn edit_denied(pool: &db::DbPool, actor: i64, issue: &Issue, pr: &PullRequest) {
    assert!(db::issues::update_issue(
        pool,
        issue.id,
        DiscussionEdit {
            actor_id: actor,
            expected_state: &issue.state,
            expected_updated: issue.updated_at,
            title: Some("Denied edit"),
            body: Some("Denied body"),
            state: None,
        }
    )
    .await
    .unwrap()
    .is_none());
    assert!(db::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        DiscussionEdit {
            actor_id: actor,
            expected_state: &pr.state,
            expected_updated: pr.updated_at,
            title: Some("Denied edit"),
            body: Some("Denied body"),
            state: None,
        },
        None,
    )
    .await
    .unwrap()
    .is_none());
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
        orgs::create_organization(
            d.pool(),
            owner,
            &orgs::NewOrganization::new(
                "contribution_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for (actor, role) in [(admin, OrgRole::Admin), (member, OrgRole::Member)] {
        applied(
            orgs::add_organization_member(d.pool(), owner, org, actor, role)
                .await
                .unwrap(),
        );
    }
    let public = d.repo(owner, Some(org), "contribution_public", false).await;
    let private = d.repo(owner, Some(org), "contribution_private", true).await;
    let personal = d.repo(owner, None, "contribution_personal", true).await;
    let foreign = d.repo(other, None, "contribution_foreign", true).await;
    let issue = db::issues::create_issue(d.pool(), private.id, owner, "Original", "Fixture")
        .await
        .unwrap();
    let pr = new_pr(d.pool(), private.id, owner).await.unwrap();
    let public_issue = db::issues::create_issue(d.pool(), public.id, owner, "Original", "Fixture")
        .await
        .unwrap();
    let public_pr = new_pr(d.pool(), public.id, owner).await.unwrap();
    let personal_issue =
        db::issues::create_issue(d.pool(), personal.id, owner, "Original", "Fixture")
            .await
            .unwrap();
    let personal_pr = new_pr(d.pool(), personal.id, owner).await.unwrap();
    let foreign_issue =
        db::issues::create_issue(d.pool(), foreign.id, other, "Original", "Fixture")
            .await
            .unwrap();
    let foreign_pr = new_pr(d.pool(), foreign.id, other).await.unwrap();
    for repo in [private.id, public.id, personal.id] {
        db::repositories::access::grant_repo_access(d.pool(), writer, repo, AccessLevel::Write)
            .await
            .unwrap();
    }
    let review = db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    let public_review = db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        public_pr.id,
        writer,
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
    let personal_review = db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        personal_pr.id,
        writer,
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    db::pull_requests::reviews::dismiss_pull_request_review(
        d.pool(),
        personal_review.id,
        site,
        Some("Personal policy"),
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(d.pool(), reader, private.id, AccessLevel::Read)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(d.pool(), reader, personal.id, AccessLevel::Read)
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
    // the actual named DAO boundary, without fabricating a repository scope.
    contributions(
        &d,
        foreign.id,
        reader,
        foreign_issue.id,
        foreign_pr.id,
        false,
    )
    .await;
    refusal(db::issues::add_issue_comment(d.pool(), i64::MAX, reader, "Missing").await);
    refusal(
        db::pull_requests::reviews::add_pull_request_comment(
            d.pool(),
            comment(i64::MAX, reader, None),
        )
        .await,
    );
    contributions(&d, private.id, i64::MAX, issue.id, pr.id, false).await;
    refusal(new_pr(d.pool(), i64::MAX, reader).await);
    refusal(db::issues::create_issue(d.pool(), i64::MAX, reader, "Missing", "Missing").await);
    let parent = db::pull_requests::reviews::add_pull_request_comment(
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
        db::pull_requests::reviews::add_pull_request_comment(
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
    assert!(db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        owner,
        "approved",
        None,
        Some(SHA)
    )
    .await
    .is_err());
    let before = d.snapshot(private.id, issue.id, pr.id).await;
    for target in [reader, other, i64::MAX] {
        refusal(
            db::pull_requests::reviews::request_pull_request_reviewer(
                d.pool(),
                pr.id,
                owner,
                target,
            )
            .await,
        );
        assert_eq!(d.snapshot(private.id, issue.id, pr.id).await, before);
    }
    db::pull_requests::reviews::request_pull_request_reviewer(d.pool(), pr.id, owner, writer)
        .await
        .unwrap();
    let requested = db::pull_requests::reviews::list_pull_request_reviewers(d.pool(), pr.id)
        .await
        .unwrap();
    assert!(
        requested.iter().any(|user| user.id == writer),
        "review request must exist before the review clears it"
    );
    db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        "commented",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    assert!(
        db::pull_requests::reviews::list_pull_request_reviewers(d.pool(), pr.id)
            .await
            .unwrap()
            .is_empty()
    );
    for actor in [writer, reader] {
        refusal(
            db::pull_requests::reviews::dismiss_pull_request_review(
                d.pool(),
                review.id,
                actor,
                None,
            )
            .await,
        );
    }
    db::pull_requests::reviews::dismiss_pull_request_review(
        d.pool(),
        review.id,
        admin,
        Some("Org Admin"),
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(d.pool(), writer, private.id, AccessLevel::Triage)
        .await
        .unwrap();
    contributions(&d, private.id, writer, issue.id, pr.id, true).await;
    review_denied(&d, private.id, writer, issue.id, pr.id, review.id, reader).await;
    db::repositories::access::grant_repo_access(d.pool(), writer, private.id, AccessLevel::Write)
        .await
        .unwrap();

    // Caller-supplied CAS survives comment/review touches. Never reload and
    // substitute the revision, including when current authority is sufficient.
    let stale_issue = db::issues::get_issue_by_id(d.pool(), issue.id)
        .await
        .unwrap()
        .unwrap();
    let stale_pr = db::pull_requests::records::get_pull_request_by_id(d.pool(), pr.id)
        .await
        .unwrap()
        .unwrap();
    db::issues::add_issue_comment(d.pool(), issue.id, reader, "Invalidate revision")
        .await
        .unwrap();
    db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        "commented",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    let before = d.snapshot(private.id, issue.id, pr.id).await;
    edit_denied(d.pool(), owner, &stale_issue, &stale_pr).await;
    let current_issue = db::issues::get_issue_by_id(d.pool(), issue.id)
        .await
        .unwrap()
        .unwrap();
    let current_pr = db::pull_requests::records::get_pull_request_by_id(d.pool(), pr.id)
        .await
        .unwrap()
        .unwrap();
    // Read authorship is not an issue/PR edit permit. Comment self-edit policy is
    // separate and cannot be tested by weakening this shared merge/write gate.
    let reader_issue =
        db::issues::create_issue(d.pool(), private.id, reader, "Read author", "Fixture")
            .await
            .unwrap();
    let reader_pr = new_pr(d.pool(), private.id, reader).await.unwrap();
    let before_edits = d.snapshot(private.id, issue.id, pr.id).await;
    edit_denied(d.pool(), reader, &reader_issue, &reader_pr).await;
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
    let closed = db::pull_requests::records::update_pull_request(
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
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(closed.state, "closed");
    assert!(db::pull_requests::reviews::add_pull_request_review(
        d.pool(),
        pr.id,
        writer,
        "approved",
        None,
        Some(SHA)
    )
    .await
    .is_err());
    db::pull_requests::records::update_pull_request(
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
        None,
    )
    .await
    .unwrap()
    .unwrap();

    // Cross-tenant team facts never become grants for the current repo. Same-org
    // Read team contributions work, then current membership revocation wins.
    let team = applied(
        orgs::create_organization_team(
            d.pool(),
            owner,
            org,
            &orgs::NewTeam::new(
                "contribution_team",
                orgs::TeamProfile::new("Contribution", None, orgs::TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_team_member(
            d.pool(),
            owner,
            org,
            team,
            member,
            orgs::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::grant_organization_team_repository(
            d.pool(),
            owner,
            org,
            team,
            private.id,
            orgs::TeamRepositoryPermission::new(1).unwrap(),
        )
        .await
        .unwrap(),
    );
    contributions(&d, private.id, member, issue.id, pr.id, true).await;
    review_denied(&d, private.id, member, issue.id, pr.id, review.id, writer).await;
    applied(
        orgs::remove_organization_team_member(d.pool(), owner, org, team, member)
            .await
            .unwrap(),
    );
    contributions(&d, private.id, member, issue.id, pr.id, false).await;
    applied(
        orgs::add_organization_team_member(
            d.pool(),
            owner,
            org,
            team,
            member,
            orgs::TeamRole::Member,
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
        orgs::create_organization(
            d.pool(),
            other,
            &orgs::NewOrganization::new(
                "contribution_foreign_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
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
    db::repositories::access::revoke_repo_access(d.pool(), reader, private.id)
        .await
        .unwrap();
    contributions(&d, private.id, reader, issue.id, pr.id, false).await;
    applied(
        orgs::update_organization_member_role(d.pool(), owner, org, admin, OrgRole::Member)
            .await
            .unwrap(),
    );
    contributions(&d, private.id, admin, issue.id, pr.id, false).await;
    review_denied(&d, private.id, admin, issue.id, pr.id, review.id, writer).await;
    comment_review_refused(&d, pr.id, admin).await;
    applied(
        orgs::update_organization_member_role(d.pool(), owner, org, admin, OrgRole::Admin)
            .await
            .unwrap(),
    );
    db::repositories::access::grant_repo_access(d.pool(), reader, private.id, AccessLevel::Read)
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
        comment_review_refused(&d, pr.id, writer).await;
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
                db::repositories::access::get_user_repo_access(d.pool(), actor, public.id)
                    .await
                    .unwrap(),
                AccessLevel::Read
            );
            contributions(&d, public.id, actor, public_issue.id, public_pr.id, false).await;
            contributions(&d, private.id, actor, issue.id, pr.id, false).await;
            review_denied(&d, private.id, actor, issue.id, pr.id, review.id, reader).await;
            comment_review_refused(&d, pr.id, actor).await;
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
    comment_review_refused(&d, pr.id, writer).await;
    d.execute(
        "UPDATE repositories SET is_archived=FALSE WHERE id=$1",
        &[private.id],
    )
    .await;
    // A deleted repository takes no contributions either.
    let doomed = d.repo(owner, None, "contribution_deleted", true).await;
    let doomed_issue = db::issues::create_issue(d.pool(), doomed.id, owner, "Original", "Fixture")
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
    comment_review_refused(&d, pr.id, admin).await;
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
    d.close().await;
}

async fn file_comment(pool: &db::DbPool, pr: i64, author: i64) -> i64 {
    db::pull_requests::reviews::add_pull_request_comment(
        pool,
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pr,
            author_id: author,
            body: "Conversation",
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
    pool: &db::DbPool,
    pr: i64,
    comment: i64,
    resolved: bool,
    actor: i64,
) -> anyhow::Result<Option<db::models::PullRequestComment>> {
    db::pull_requests::reviews::set_pull_request_comment_resolved(
        pool, pr, comment, resolved, actor,
    )
    .await
}
async fn still_open(pool: &db::DbPool, comment: i64) -> bool {
    db::pull_requests::reviews::get_pull_request_comment_by_id(pool, comment)
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
    let personal = d.repo(owner, None, "resolve_personal", true).await;
    for (actor, level) in [
        (author, AccessLevel::Read),
        (writer, AccessLevel::Write),
        (revoked, AccessLevel::Write),
    ] {
        db::repositories::access::grant_repo_access(d.pool(), actor, personal.id, level)
            .await
            .unwrap();
    }
    db::repositories::access::revoke_repo_access(d.pool(), revoked, personal.id)
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
        orgs::create_organization(
            d.pool(),
            owner,
            &orgs::NewOrganization::new(
                "resolve_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let shared = d.repo(owner, Some(org), "resolve_shared", true).await;
    db::repositories::access::grant_repo_access(d.pool(), author, shared.id, AccessLevel::Read)
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
    d.close().await;
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

// Poll an actual server wait edge; elapsed time is not concurrency evidence.
async fn wait_for_lock(raw: &sqlx::PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock')")
                .bind(blocker).fetch_one(raw).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("actual lock wait edge");
}

async fn wait_for_lock_chain(raw: &sqlx::PgPool, blocker: i32, waiters: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "WITH RECURSIVE waiting(pid) AS (
                    SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))
                    UNION
                    SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid=ANY(pg_blocking_pids(a.pid))
                ) SELECT COUNT(*) FROM waiting",
            )
            .bind(blocker)
            .fetch_one(raw)
            .await
            .unwrap();
            if waiting >= waiters {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual transitive lock wait edges");
}

#[tokio::test]
async fn postgres_contributions_recheck_after_grant_revocation_before_parent_locks() {
    let mut f = production::Fixture::new().await;
    let actor = db::users::accounts::create_user(&f.pool, "contribution_wait_reader", None)
        .await
        .unwrap()
        .id;
    let target = db::users::accounts::create_user(&f.pool, "contribution_wait_target", None)
        .await
        .unwrap()
        .id;
    let org = applied(
        orgs::create_organization(
            &f.pool,
            f.owner.id,
            &orgs::NewOrganization::new(
                "contribution_wait_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
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
    db::repositories::access::grant_repo_access(&f.pool, actor, f.repo.id, AccessLevel::Admin)
        .await
        .unwrap();
    let issue = db::issues::create_issue(&f.pool, f.repo.id, f.owner.id, "Wait", "Fixture")
        .await
        .unwrap();
    let pr = new_pr(&f.pool, f.repo.id, f.owner.id).await.unwrap();
    let review = db::pull_requests::reviews::add_pull_request_review(
        &f.pool,
        pr.id,
        actor,
        "approved",
        None,
        Some(SHA),
    )
    .await
    .unwrap();
    for operation in 0..8 {
        db::repositories::access::grant_repo_access(&f.pool, actor, f.repo.id, AccessLevel::Admin)
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
                0 => db::issues::create_issue(&pool, repo_id, actor, "Denied", "Denied")
                    .await
                    .map(|_| ()),
                1 => new_pr(&pool, repo_id, actor).await.map(|_| ()),
                2 => db::issues::add_issue_comment(&pool, issue_id, actor, "Denied")
                    .await
                    .map(|_| ()),
                3 => db::pull_requests::reviews::add_pull_request_comment(
                    &pool,
                    comment(pr_id, actor, None),
                )
                .await
                .map(|_| ()),
                4 => db::pull_requests::reviews::add_pull_request_review(
                    &pool,
                    pr_id,
                    actor,
                    "approved",
                    None,
                    Some(SHA),
                )
                .await
                .map(|_| ()),
                5 => db::pull_requests::reviews::request_pull_request_reviewer(
                    &pool, pr_id, actor, target,
                )
                .await
                .map(|_| ()),
                6 => db::pull_requests::reviews::remove_pull_request_reviewer(
                    &pool, pr_id, actor, target,
                )
                .await
                .map(|_| ()),
                _ => db::pull_requests::reviews::dismiss_pull_request_review(
                    &pool, review_id, actor, None,
                )
                .await
                .map(|_| ()),
            }
        });
        wait_for_lock(&f.raw, pid).await;
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
    db::repositories::access::grant_repo_access(&f.pool, actor, f.repo.id, AccessLevel::Read)
        .await
        .unwrap();
    let accepted = db::issues::add_issue_comment(&f.pool, issue.id, actor, "Accepted snapshot")
        .await
        .unwrap();
    db::repositories::access::revoke_repo_access(&f.pool, actor, f.repo.id)
        .await
        .unwrap();
    refusal(db::issues::add_issue_comment(&f.pool, issue.id, actor, "Replay").await);
    assert!(db::issues::list_issue_comments(&f.pool, issue.id, 100, 0)
        .await
        .unwrap()
        .iter()
        .any(|c| c.comment.id == accepted.id && c.comment.body == "Accepted snapshot"));
    f.finish().await;
}

#[tokio::test]
async fn postgres_contribution_keeps_its_grant_until_the_effect_commits() {
    let mut f = production::Fixture::new().await;
    let actor = db::users::accounts::create_user(&f.pool, "contribution_commit_reader", None)
        .await
        .unwrap()
        .id;
    db::repositories::access::grant_repo_access(&f.pool, actor, f.repo.id, AccessLevel::Read)
        .await
        .unwrap();
    let issue = db::issues::create_issue(&f.pool, f.repo.id, f.owner.id, "Commit order", "Fixture")
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
    let issue_id = issue.id;
    let effect = tokio::spawn(async move {
        db::issues::add_issue_comment(&pool, issue_id, actor, "Accepted before revocation").await
    });
    wait_for_lock(&f.raw, blocker).await;

    let pool = f.pool.clone();
    let repo = f.repo.id;
    let revocation = tokio::spawn(async move {
        db::repositories::access::revoke_repo_access(&pool, actor, repo).await
    });
    wait_for_lock_chain(&f.raw, blocker, 2).await;
    assert!(!revocation.is_finished());
    parent.commit().await.unwrap();

    let comment = effect.await.unwrap().unwrap();
    assert!(revocation.await.unwrap().unwrap());
    assert_eq!(comment.body, "Accepted before revocation");
    refusal(db::issues::add_issue_comment(&f.pool, issue.id, actor, "Denied after").await);
    f.finish().await;
}

#[tokio::test]
async fn postgres_team_contributions_wait_for_each_revoked_authority_row() {
    let mut f = production::Fixture::new().await;
    let actor = db::users::accounts::create_user(&f.pool, "team_wait_reader", None)
        .await
        .unwrap()
        .id;
    let org = applied(
        orgs::create_organization(
            &f.pool,
            f.owner.id,
            &orgs::NewOrganization::new(
                "team_wait_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
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
    let issue = db::issues::create_issue(&f.pool, f.repo.id, f.owner.id, "Wait", "Fixture")
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
        let issue_id = issue.id;
        let task = tokio::spawn(async move {
            db::issues::add_issue_comment(&pool, issue_id, actor, "Denied").await
        });
        wait_for_lock(&f.raw, pid).await;
        revoke.commit().await.unwrap();
        refusal(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(db::issues::list_issue_comments(&f.pool, issue.id, 100, 0)
        .await
        .unwrap()
        .is_empty());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_contributions_use_reserved_writer_snapshot() {
    let f = native::NativeDatabase::new().await;
    let owner = db::users::accounts::create_user(&f.pool, "sqlite_contribution_owner", None)
        .await
        .unwrap()
        .id;
    let actor = db::users::accounts::create_user(&f.pool, "sqlite_contribution_reader", None)
        .await
        .unwrap()
        .id;
    let repo = db::repositories::records::create_repository(
        &f.pool,
        owner,
        "sqlite_contribution",
        None,
        true,
        "/owned/not-executed.git",
    )
    .await
    .unwrap();
    let issue = db::issues::create_issue(&f.pool, repo.id, owner, "Reserved", "Fixture")
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(&f.pool, actor, repo.id, AccessLevel::Read)
        .await
        .unwrap();
    let accepted = db::issues::add_issue_comment(&f.pool, issue.id, actor, "Accepted snapshot")
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
    refusal(db::issues::add_issue_comment(&second, issue.id, actor, "Revoked snapshot").await);
    let comments = db::issues::list_issue_comments(&second, issue.id, 100, 0)
        .await
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].comment.id, accepted.id);
    second.close().await;
    f.close().await;
}
