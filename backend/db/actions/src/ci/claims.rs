use anyhow::ensure;
use anyhow::Result;

pub const DEFAULT_CANDIDATE_LIMIT: i64 = 128;

/// Maximum compatible queue candidates returned to one claim's authority checks.
/// This is not a database scan budget: incompatible/blocked rows are filtered before
/// LIMIT, so the query may examine the entire queued backlog on either engine.
pub fn candidate_limit() -> Result<i64> {
    let limit = match std::env::var("CI_CLAIM_CANDIDATES") {
        Ok(value) => value
            .parse::<i64>()
            .context("CI_CLAIM_CANDIDATES must be an integer")?,
        Err(std::env::VarError::NotPresent) => DEFAULT_CANDIDATE_LIMIT,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (1..=1024).contains(&limit),
        "CI_CLAIM_CANDIDATES must be between 1 and 1024"
    );
    Ok(limit)
}

/// Scheduling is a prefilter, not authority: claims repeat live checks under their locks.
/// Parameters are node key, execution mode, platform labels, cursor and candidate limit.
pub(crate) fn candidates_sql(pg: bool) -> String {
    let managed = "COALESCE(n.runner_managed,FALSE)";
    let wanted = if pg {
        "jsonb_array_elements_text(c.runner_labels::jsonb)"
    } else {
        "json_each(c.runner_labels)"
    };
    let platform = if pg {
        "$3::jsonb ? wanted.value"
    } else {
        "EXISTS(SELECT 1 FROM json_each($3) platform WHERE platform.value=wanted.value)"
    };
    let runner_label = if pg {
        "a.labels::jsonb ? wanted.value"
    } else {
        "EXISTS(SELECT 1 FROM json_each(a.labels) label WHERE label.value=wanted.value)"
    };
    let node_runner = if pg {
        "'runner'=ANY(n.roles)"
    } else {
        "EXISTS(SELECT 1 FROM json_each(n.roles) role WHERE role.value='runner')"
    };
    let blockers = crate::action_concurrency::blockers_sql(true)
        .trim_start_matches("SELECT ")
        .replace("$1", "request.repo_id")
        .replace("$2", "request.name_key")
        .replace("$3", "request.request_order")
        .replace("$4", "request.run_id")
        .replace("$5", "NULL");
    let lock = if pg {
        "FOR UPDATE OF c SKIP LOCKED"
    } else {
        ""
    };
    let sql = format!(
        "SELECT c.* FROM ci_runs c JOIN repositories r ON r.id=c.repo_id
         LEFT JOIN nodes n ON n.public_key=$1
         LEFT JOIN action_runners a ON a.node_id=n.id
         LEFT JOIN action_runner_groups g ON g.id=a.group_id AND g.org_id=a.org_id
         WHERE c.status='queued' AND NOT c.approval_required AND c.id>$4
         AND ($1 IS NULL OR (n.status='active' AND {node_runner}))
         AND NOT EXISTS(SELECT 1 FROM {wanted} wanted WHERE NOT (
             (NOT {managed} AND (({platform}) OR ($2='local' AND wanted.value='self-hosted')))
             OR ({managed} AND (wanted.value='self-hosted' OR ({runner_label}))
                 AND ($2='local' OR ({platform})))))
         AND ((NOT {managed} AND c.runner_group IS NULL)
             OR ({managed} AND a.id IS NOT NULL AND NOT a.used AND (
                 (a.repo_id=c.repo_id AND c.runner_group IS NULL)
                 OR (r.owner_kind='org' AND r.owner_user_id IS NULL AND a.org_id=r.owner_org_id
                     AND (c.runner_group IS NULL OR lower(c.runner_group)=lower(g.name))
                     AND (r.is_private OR g.allows_public_repositories)
                     AND (g.visibility='all' OR (g.visibility='private' AND r.is_private)
                         OR (g.visibility='selected' AND EXISTS(SELECT 1 FROM action_runner_group_repositories s WHERE s.group_id=g.id AND s.repo_id=c.repo_id)))))))
         AND NOT EXISTS(SELECT 1 FROM actions_run_concurrency request WHERE request.run_id=c.id AND ({blockers}))
         ORDER BY c.id LIMIT $5 {lock}"
    );
    if pg {
        sql
    } else {
        sql.replace('$', "?")
    }
}
