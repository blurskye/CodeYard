use anyhow::Result;
use codeyard_db_core::queries::ci::CiCredential;
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_core::queries::postgres::credentials::repository;
use codeyard_db_core::queries::users::accounts::OwnedAccountCredential;
use codeyard_support::credentials::AccountRestricted;
use sqlx::Postgres;
use sqlx::Transaction;

/// A live, unrestricted account and, unless this is a trusted in-process call, its live credential.
pub async fn require_account(tx: &mut Transaction<'_, Postgres>, actor: &Actor) -> Result<()> {
    match actor.credential() {
        Some(credential) => {
            crate::postgres::users::transitions::require_account(tx, actor.user_id, &credential)
                .await
        }
        None => {
            anyhow::ensure!(
                crate::postgres::users::transitions::active_account(tx, actor.user_id).await?,
                AccountRestricted
            );
            Ok(())
        }
    }
}

/// The credential behind a CI request, checked in the transaction that makes the effect.
/// A job token acts for the user who triggered its running run in this repository; any
/// other credential must belong to `actor`. Repository access is the caller's check.
pub async fn ci(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    proof: &CiCredential,
) -> Result<bool> {
    let (owner, credential, permission) = match proof {
        CiCredential::Session(session) => (
            actor,
            OwnedAccountCredential::Session(session.clone()),
            "actions",
        ),
        CiCredential::ForeignToken {
            id,
            hash,
            permission,
        } => (
            actor,
            OwnedAccountCredential::ForeignToken(*id, hash.clone()),
            *permission,
        ),
        CiCredential::Token {
            id,
            hash,
            permission,
        } => {
            let token: Option<(i64, bool)> = sqlx::query_as("SELECT t.user_id,u.username=$2 FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1")
                .bind(id).bind(codeyard_db_core::queries::ci_tokens::BOT).fetch_optional(&mut **tx).await?;
            let Some((owner, bot)) = token else {
                return Ok(false);
            };
            let job: Option<bool> = sqlx::query_scalar("SELECT r.repo_id=$2 AND r.triggered_by_id=$3 AND r.status='running' AND NOT r.cancel_requested AND g.revoked_at IS NULL FROM actions_github_tokens g JOIN ci_runs r ON r.id=g.run_id WHERE g.token_id=$1 FOR SHARE OF g,r")
                .bind(id).bind(repo).bind(actor).fetch_optional(&mut **tx).await?;
            // Only the workflow bot holds job tokens, and it holds no other kind.
            if bot != job.is_some() || job == Some(false) {
                return Ok(false);
            }
            (
                if bot { owner } else { actor },
                OwnedAccountCredential::Token(*id, hash.clone()),
                *permission,
            )
        }
    };
    if owner != actor {
        // A job bot is discovered after the triggering actor's resource locks.
        // Never wait for a restriction in the reverse account order.
        let retained: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock_shared(hashtextextended(COALESCE(current_schema(),'') || ':account:' || $1::bigint::text,0))")
            .bind(owner).fetch_one(&mut **tx).await?;
        if !retained {
            return Ok(false);
        }
    }
    let actor = Actor {
        user_id: owner,
        credential: Some(credential),
    };
    match require_account(tx, &actor).await {
        Ok(()) => {}
        Err(error) if error.is::<AccountRestricted>() => return Ok(false),
        Err(error) => return Err(error),
    }
    repository(tx, &actor, repo, permission, true).await
}
