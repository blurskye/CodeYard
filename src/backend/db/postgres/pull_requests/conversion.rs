use crate::backend::db::{
    models::{Issue, PullRequest},
    pull_requests::records::IssueConversionRefusal,
};
use anyhow::Result;
use sqlx::{Postgres, Transaction};

pub(super) async fn read_source(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    number: i32,
) -> Result<Issue> {
    anyhow::ensure!(
        super::super::discussions::lock_write_authority(tx, actor, repo).await?,
        super::super::discussions::AUTHORITY_DENIED
    );
    let issue = sqlx::query_as::<_, Issue>(
        "SELECT * FROM issues WHERE repo_id=$1 AND number=$2 FOR UPDATE",
    )
    .bind(repo)
    .bind(number)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IssueConversionRefusal::Missing)?;
    anyhow::ensure!(
        number > 0 && matches!(issue.state.as_str(), "open" | "closed"),
        IssueConversionRefusal::Invalid
    );
    anyhow::ensure!(!issue.locked, IssueConversionRefusal::Locked);
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pull_requests WHERE repo_id=$1 AND number=$2)",
    )
    .bind(repo)
    .bind(number)
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(!used, IssueConversionRefusal::NumberInUse);
    let metadata: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM issue_labels il JOIN labels l ON l.id=il.label_id WHERE il.issue_id=$1 AND l.repo_id<>$2) AND ($3::BIGINT IS NULL OR EXISTS(SELECT 1 FROM milestones WHERE id=$3 AND repo_id=$2))")
        .bind(issue.id).bind(repo).bind(issue.milestone_id).fetch_one(&mut **tx).await?;
    anyhow::ensure!(metadata, IssueConversionRefusal::Invalid);
    // The creation lock excludes new issue metadata. PRs have no equivalents for these
    // records, so conversion must not silently cascade-delete them.
    let portable: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM issue_type_assignments WHERE issue_id=$1) AND NOT EXISTS(SELECT 1 FROM issue_field_values WHERE issue_id=$1) AND NOT EXISTS(SELECT 1 FROM issue_relations WHERE kind IN ('parent','blocks','related') AND source_id=$1) AND NOT EXISTS(SELECT 1 FROM issue_relations WHERE kind IN ('parent','blocks','related') AND target_id=$1) AND NOT EXISTS(SELECT 1 FROM issue_suggestions WHERE issue_id=$1)")
        .bind(issue.id).fetch_one(&mut **tx).await?;
    anyhow::ensure!(portable, IssueConversionRefusal::Invalid);
    Ok(issue)
}

/// Both the source and its canonical replacement are protected by the repository's existing
/// exclusive creation lock. Reactions and timeline rows keep their native IDs and authorship.
pub(super) async fn move_source(
    tx: &mut Transaction<'_, Postgres>,
    issue: &Issue,
    pull: &PullRequest,
    actor: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO pr_labels(pull_request_id,label_id) SELECT $1,label_id FROM issue_labels WHERE issue_id=$2")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO pr_assignees(pull_request_id,user_id) SELECT $1,user_id FROM issue_assignees WHERE issue_id=$2")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO pr_comments(pull_request_id,author_id,body,created_at,updated_at,source_issue_comment_id) SELECT $1,author_id,body,created_at,updated_at,id FROM issue_comments WHERE issue_id=$2 ORDER BY created_at,id")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO pr_comment_pins(pull_request_id,comment_id,pinned_by,pinned_at) SELECT $1,c.id,p.pinned_by,p.pinned_at FROM issue_comment_pins p JOIN pr_comments c ON c.pull_request_id=$1 AND c.source_issue_comment_id=p.comment_id WHERE p.issue_id=$2")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("UPDATE reactions SET subject_kind='pr_comment',subject_id=(SELECT c.id FROM pr_comments c WHERE c.pull_request_id=$1 AND c.source_issue_comment_id=reactions.subject_id) WHERE subject_kind='issue_comment' AND subject_id IN (SELECT id FROM issue_comments WHERE issue_id=$2)")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("UPDATE reactions SET subject_kind='pull_request',subject_id=$1 WHERE subject_kind='issue' AND subject_id=$2")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    sqlx::query("UPDATE timeline_events SET subject_kind='pull_request',subject_id=$1 WHERE subject_kind='issue' AND subject_id=$2")
        .bind(pull.id).bind(issue.id).execute(&mut **tx).await?;
    if issue.state == "closed" {
        super::super::timeline::append(tx, pull.repo_id, "pull_request", pull.id, actor, "reopened", "", "").await?;
    }
    super::super::timeline::append(tx, pull.repo_id, "pull_request", pull.id, actor, "converted", "", "").await?;
    // Project entries move to the new pull request, keeping their fields, archive state and position.
    sqlx::query("UPDATE project_items SET issue_id=NULL,pull_request_id=$1 WHERE issue_id=$2")
        .bind(pull.id)
        .bind(issue.id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM issues WHERE id=$1 AND repo_id=$2")
        .bind(issue.id)
        .bind(pull.repo_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
