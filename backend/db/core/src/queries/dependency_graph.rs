use codeyard_model::dependency_graph::GraphError;
/// One reported default-branch revision, ordered by server receipt rather than detector clocks.
/// Callers bind the repository as $1; all detectors contributing alerts must use this SHA.
pub const DEFAULT_SNAPSHOT_SHA_SQL: &str = "(SELECT d.sha FROM dependency_snapshots d JOIN repositories r ON r.id=d.repo_id WHERE d.repo_id=$1 AND d.reference='refs/heads/'||r.default_branch ORDER BY d.id DESC LIMIT 1)";

/// Whether the dependency graph is on, as on GitHub: an applied setting decides; without one it
/// is on for a public repository, and on for a private one only once Dependabot alerts are
/// enabled for it with the owner's consent to read its private dependencies.
pub fn enabled_sql(pg: bool, repo: &str) -> String {
    let status = if pg {
        "(g.security_and_analysis::jsonb)->'dependency_graph'->>'status'"
    } else {
        "json_extract(g.security_and_analysis,'$.dependency_graph.status')"
    };
    format!("CASE (SELECT {status} FROM repository_security_settings g WHERE g.repo_id={repo}) WHEN 'disabled' THEN FALSE WHEN 'enabled' THEN TRUE ELSE EXISTS(SELECT 1 FROM repositories p WHERE p.id={repo} AND (NOT p.is_private OR EXISTS(SELECT 1 FROM repository_dependency_security d WHERE d.repo_id=p.id AND d.alerts_enabled AND d.private_consent))) END")
}

pub(crate) const MAX_SNAPSHOT_SOURCES: usize = 100;

pub(crate) const MAX_SNAPSHOT_BYTES: i64 = 4 * 1024 * 1024;

pub(crate) fn snapshot_candidates_sql(pg: bool, reference: bool, sha: bool) -> String {
    let reference = if reference {
        "reference=$2"
    } else {
        "CAST($2 AS TEXT) IS NULL"
    };
    let sha = if sha {
        "sha=$3"
    } else {
        "CAST($3 AS TEXT) IS NULL"
    };
    let bytes = if pg {
        "octet_length(s.body)"
    } else {
        "length(CAST(s.body AS BLOB))"
    };
    let limit = MAX_SNAPSHOT_SOURCES + 1;
    format!("WITH ranked AS (SELECT id,ROW_NUMBER() OVER(PARTITION BY detector,correlator ORDER BY scanned DESC,id DESC) AS position FROM dependency_snapshots WHERE repo_id=$1 AND {reference} AND {sha}), candidates AS (SELECT id FROM ranked WHERE position=1 ORDER BY id LIMIT {limit}) SELECT s.id,CAST({bytes} AS BIGINT) FROM candidates c JOIN dependency_snapshots s ON s.id=c.id ORDER BY s.id")
}

pub(crate) fn snapshot_ids(rows: Vec<(i64, i64)>) -> Result<Vec<i64>, GraphError> {
    if rows.len() > MAX_SNAPSHOT_SOURCES
        || rows.iter().map(|(_, bytes)| bytes).sum::<i64>() > MAX_SNAPSHOT_BYTES
    {
        return Err(GraphError::Limit);
    }
    Ok(rows.into_iter().map(|(id, _)| id).collect())
}
