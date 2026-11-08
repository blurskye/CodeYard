use anyhow::Result;
/// Called after resource authority has been locked, in the transaction that writes it.
/// The token row lock serializes revocation with the effect, not just HTTP authentication.
pub async fn permits_write(
    tx: &mut sqlx::PgConnection,
    actor: i64,
    token: i64,
    repo: Option<i64>,
    org: Option<i64>,
    permission: &str,
) -> Result<bool> {
    crate::queries::postgres::users::transitions::lock_accounts_shared(tx, &[actor]).await?;
    crate::queries::postgres::oauth_apps::lock_token_issuer(tx, token).await?;
    let live: Option<String> = sqlx::query_scalar("SELECT u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND u.id=$2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF t")
        .bind(token).bind(actor).fetch_optional(&mut *tx).await?;
    let Some(username) = live else {
        return Ok(false);
    };
    let repository: Option<crate::models::Repository> = match repo {
        Some(repo) => {
            sqlx::query_as("SELECT * FROM repositories WHERE id=$1")
                .bind(repo)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let org = org.or_else(|| repository.as_ref().and_then(|r| r.owner_org_id));
    let bounds: Option<crate::queries::fine_grained_tokens::Bounds> = sqlx::query_as(&format!(
        "{} FOR SHARE OF f",
        crate::queries::postgres::fine_grained_tokens::BOUNDS
    ))
    .bind(token)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(bounds) = bounds {
        let active = bounds.active();
        let permissions: serde_json::Value = serde_json::from_str(&bounds.permissions)?;
        let reaches = if let Some(repository) = &repository {
            let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=$1 AND repo_id=$2)")
                .bind(token).bind(repository.id).fetch_one(&mut *tx).await?;
            (bounds.owner_org_id.is_some() && repository.owner_org_id == bounds.owner_org_id
                || bounds.owner_org_id.is_none() && repository.owner_user_id == Some(actor))
                && (bounds.repository_access == "all"
                    || bounds.repository_access == "selected" && selected)
        } else {
            org.is_some() && bounds.owner_org_id == org
        };
        return Ok(active
            && reaches
            && codeyard_types::github_apps::allows(&permissions, permission, true));
    }
    let app_token: Option<crate::queries::github_apps::Token> =
        sqlx::query_as("SELECT * FROM github_app_tokens WHERE token_id=$1 FOR SHARE")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await?;
    let oauth: Option<(String, String, bool)> = sqlx::query_as("SELECT o.client_id,o.scopes,COALESCE(c.is_github_app,FALSE) FROM oauth_app_tokens o LEFT JOIN oauth_applications c ON c.client_id=o.client_id WHERE o.token_id=$1")
        .bind(token).fetch_optional(&mut *tx).await?;
    if app_token.is_some() || oauth.as_ref().is_some_and(|o| o.2) {
        let Some(repo) = repo else { return Ok(false) };
        let (installation, permissions, selected) = if let Some(bound) = app_token {
            let selected: Vec<i64> = serde_json::from_str(&bound.repositories)?;
            (Some(bound.installation_id), bound.permissions, selected)
        } else {
            (None, String::new(), Vec::new())
        };
        let client = oauth.as_ref().map(|o| o.0.as_str());
        let grant: Option<String> = sqlx::query_scalar("SELECT a.permissions FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=$1 WHERE ($2::BIGINT IS NULL OR i.id=$2) AND ($2::BIGINT IS NOT NULL OR a.client_id=$3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) FOR SHARE OF i,a")
            .bind(repo).bind(installation).bind(client).fetch_optional(&mut *tx).await?;
        let Some(grant) = grant else { return Ok(false) };
        let grant: serde_json::Value = serde_json::from_str(&grant)?;
        let permissions: serde_json::Value = if installation.is_some() {
            serde_json::from_str(&permissions)?
        } else {
            grant.clone()
        };
        // App environment administration and fine-grained environment grants use different names.
        let permission = if permission == "environments" {
            "administration"
        } else {
            permission
        };
        return Ok((selected.is_empty() || selected.contains(&repo))
            && codeyard_types::github_apps::allows(&permissions, permission, true)
            && codeyard_types::github_apps::allows(&grant, permission, true));
    }
    if username == crate::queries::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running' FOR SHARE OF t,r")
            .bind(token).fetch_optional(&mut *tx).await?;
        return match job {
            Some((id, permissions)) => Ok(repo == Some(id)
                && serde_json::from_str::<codeyard_types::permissions::Permissions>(&permissions)?
                    .allows(&permission.replace('_', "-"), true)),
            None => Ok(false),
        };
    }
    if let Some((client, scopes, _)) = oauth {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications c JOIN users owner ON owner.id=c.owner_id WHERE c.client_id=$1 AND NOT owner.is_banned AND NOT owner.must_change_password)")
            .bind(&client).fetch_one(&mut *tx).await?;
        let allowed = codeyard_types::oauth_apps::has_scope(
            &scopes,
            if repo.is_some() { "repo" } else { "write:org" },
        ) || permission == "security_events"
            && codeyard_types::oauth_apps::has_scope(&scopes, "security_events");
        return Ok((active || client == codeyard_types::identity::CLI_CLIENT) && allowed);
    }
    let refused: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=$1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut *tx).await?;
    Ok(!refused)
}
