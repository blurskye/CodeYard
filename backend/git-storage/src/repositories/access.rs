//! Current repository read, write and administration decisions.

use anyhow::Result;
use codeyard_database::models::Repository;
use codeyard_identity::repository_access::can_write;

/// Recheck the credential retained by a native publication, after preparation and lock waits.
pub async fn check_publication_actor(
    pool: &codeyard_database::DbPool,
    actor: &codeyard_database::queries::credentials::Actor,
    repository: &Repository,
) -> Result<()> {
    use codeyard_database::queries::users::accounts::OwnedAccountCredential;
    let repository =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!(codeyard_types::credentials::AccountRestricted))?;
    let user = match &actor.credential {
        None => codeyard_db_identity::users::accounts::get_user_by_id(pool, actor.user_id).await?,
        Some(OwnedAccountCredential::Session(session)) => {
            codeyard_db_identity::users::sessions::get_session_user(pool, session).await?
        }
        Some(OwnedAccountCredential::ForeignToken(id, hash)) => {
            let credential = codeyard_database::cells::authorization::lookup(
                pool,
                &codeyard_database::cells::authorization::Query {
                    kind: "token",
                    id: Some(*id),
                    hash: Some(hash),
                    write: true,
                },
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!(codeyard_types::credentials::AccountRestricted))?;
            anyhow::ensure!(
                credential.answer.authority.repository(
                    credential.user.id,
                    &repository,
                    "contents",
                    true
                )?,
                codeyard_types::credentials::AccountRestricted
            );
            Some(credential.user)
        }
        Some(OwnedAccountCredential::Token(token_id, _)) => Some(
            crate::repositories::git_sessions::ReceiveCredential::Token(*token_id)
                .authorize(pool, &repository, true)
                .await?
                .0,
        ),
    }
    .ok_or_else(|| anyhow::anyhow!(codeyard_types::credentials::AccountRestricted))?;
    anyhow::ensure!(
        user.id == actor.user_id && can_write(pool, &user, &repository).await?,
        codeyard_types::credentials::AccountRestricted
    );
    Ok(())
}
