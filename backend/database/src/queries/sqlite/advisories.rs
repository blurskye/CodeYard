use anyhow::Result;
use codeyard_types::advisories::Advisory;
use codeyard_types::advisories::Refusal;
pub async fn administration(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    repo: i64,
    ghsa: &str,
) -> Result<Advisory> {
    anyhow::ensure!(
        crate::queries::sqlite::discussions::lock_security_authority(tx, actor, repo, true).await?,
        Refusal::Denied
    );
    Ok(
        sqlx::query_as("SELECT * FROM repository_advisories WHERE repo_id=?1 AND ghsa_id=?2")
            .bind(repo)
            .bind(ghsa)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Refusal::Missing)?,
    )
}
