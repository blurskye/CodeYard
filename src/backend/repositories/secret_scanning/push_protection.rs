//! Push protection refuses a publication that adds a secret, unless the actor holds an unused
//! bypass for that exact secret. Receive hooks and file saves both run it on the commits they add.

use crate::backend::db;
use crate::backend::repositories::store::{ErrorKind, RepoError};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct ApprovedBypass {
    pub hash: String,
    pub commit: String,
}

/// Scans the commits that `heads` add and refuses the publication when one holds a secret that
/// push protection blocks. Returns the hashes of the secrets that the actor's bypass allows. The
/// caller records their use once the publication has succeeded.
pub async fn refuse_blocked_secrets(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    actor: i64,
    path: &Path,
    quarantine: Option<&Path>,
    heads: &[String],
) -> anyhow::Result<Vec<ApprovedBypass>> {
    if !db::secret_scanning::settings(pool, repo.id)
        .await?
        .push_protection
    {
        return Ok(Vec::new());
    }
    let owner = db::repositories::records::repository_owner_names(pool, &[repo.id])
        .await?
        .remove(&repo.id)
        .ok_or_else(|| anyhow::anyhow!("Repository owner unavailable"))?;
    let (patterns, disabled) = db::secret_scanning::effective_patterns(pool, repo.id).await?;
    let findings = crate::backend::git::secret_scanning::added_secrets(
        path.to_owned(),
        quarantine.map(Path::to_owned),
        heads.to_vec(),
        patterns,
        disabled,
    )
    .await?;
    let mut violations = Vec::new();
    let mut bypasses = Vec::new();
    for finding in findings {
        let hash = crate::backend::identity::tokens::token_hash(&finding.detection.secret);
        if let Some(placeholder) = db::secret_scanning::protection(
            pool,
            actor,
            repo.id,
            &hash,
            &finding.detection.secret_type,
        )
        .await?
        {
            let link = format!(
                "{}/{}/{}/security/secret-scanning/unblock-secret/{}",
                crate::backend::api::http::config::link_origin(),
                codeyard_webui::encode_component(&owner),
                codeyard_webui::encode_component(&repo.name),
                placeholder.id
            );
            let safe_path = finding
                .location
                .path
                .replace(&finding.detection.secret, "[REDACTED]");
            violations.push(format!("  —— {} ({}) ——\n    locations:\n      - commit: {}\n        path: {}:{}\n    To push, remove secret from commit(s) or follow this URL to allow the secret.\n    {}", finding.detection.secret_type_display_name, finding.detection.rule_id, finding.location.commit_sha, safe_path.escape_debug(), finding.location.start_line, link));
        } else {
            bypasses.push(ApprovedBypass {
                hash,
                commit: finding.location.commit_sha,
            });
        }
    }
    if !violations.is_empty() {
        return Err(RepoError::new(ErrorKind::Invalid,format!("GH013: Repository rule violations found\n\n- PUSH PROTECTION\n  Push cannot contain secrets\n\n{}", violations.join("\n\n"))).into());
    }
    Ok(bypasses)
}
