use crate::queries::secrets::SecretsError;
/// Call under the owning organization/repository authority locks, after the proposed edit.
/// Ciphertext length is plaintext length plus the fixed AES-GCM tag; no decryption needed.
pub async fn delivery_budget(c: &mut sqlx::PgConnection, repo: i64) -> Result<(), SecretsError> {
    let (count, bytes): (i64, i64) = sqlx::query_as("WITH inherited AS (SELECT v.name,octet_length(v.name)+octet_length(v.encrypted_value)-16 AS bytes FROM org_actions_values v JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=v.org_id WHERE r.id=$1 AND v.kind='secret' AND (v.visibility='all' OR (v.visibility='private' AND r.is_private) OR (v.visibility='selected' AND EXISTS(SELECT 1 FROM org_actions_value_repositories s WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name AND s.repo_id=r.id))) ORDER BY v.name LIMIT 100), delivered AS (SELECT i.bytes FROM inherited i WHERE NOT EXISTS(SELECT 1 FROM repo_secrets s WHERE s.repo_id=$1 AND upper(s.name)=upper(i.name)) UNION ALL SELECT octet_length(name)+octet_length(encrypted_value)-16 FROM repo_secrets WHERE repo_id=$1) SELECT COUNT(*),COALESCE(SUM(bytes),0)::BIGINT FROM delivered")
        .bind(repo).fetch_one(c).await?;
    crate::queries::secrets::ensure_workflow_budget(count, bytes)
}
