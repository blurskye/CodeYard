//! Repository credential bounds checked inside the owning transaction.
use crate::backend::db::users::accounts::AccountCredential;

pub struct Reader<'a> {
    pub user: Option<i64>,
    pub credential: Option<AccountCredential<'a>>,
}
impl<'a> Reader<'a> {
    pub fn authenticated(user: i64, credential: &AccountCredential<'a>) -> Self {
        Self {
            user: Some(user),
            credential: Some(match credential {
                AccountCredential::Session(session) => AccountCredential::Session(session),
                AccountCredential::Token(id, hash) => AccountCredential::Token(*id, hash),
            }),
        }
    }
    pub(crate) fn token(&self) -> Option<i64> {
        match &self.credential { Some(AccountCredential::Token(id, _)) => Some(*id), _ => None }
    }
    pub(crate) fn hash(&self) -> Option<&str> {
        match &self.credential { Some(AccountCredential::Token(_, hash)) => Some(*hash), _ => None }
    }
    pub(crate) fn session(&self) -> Option<&str> {
        match &self.credential { Some(AccountCredential::Session(session)) => Some(*session), _ => None }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Access {
    /// Public metadata may be read outside a fine-grained token's repository bounds.
    PublicRead,
    /// Collaborator-only data requires credential bounds even in a public repository.
    Read,
    Write,
}

/// `r` and `u` are the repository and viewer. Placeholders, never caller text, name the credential.
pub(crate) fn repository_condition(postgres: bool, scope: &str, access: Access, token: &str, session: &str, hash: &str) -> String {
    let write = matches!(access, Access::Write);
    let public_read = matches!(access, Access::PublicRead);
    let levels = if write { "'write','admin'" } else { "'read','write','admin'" };
    let public = if public_read { "NOT r.is_private" } else { "FALSE" };
    let token_scope = if write { "t.scope='write'" } else { "TRUE" };
    let now = if postgres { "clock_timestamp()" } else { "strftime('%Y-%m-%dT%H:%M:%fZ','now')" };
    let epoch = if postgres { "FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint" } else { "CAST(strftime('%s','now') AS BIGINT)" };
    let recent = if postgres { "clock_timestamp()-INTERVAL '1 minute'" } else { "strftime('%Y-%m-%dT%H:%M:%fZ','now','-1 minute')" };
    let permission = |document: &str, key: &str| if postgres {
        format!("({document})::jsonb->>'{key}' IN ({levels})")
    } else {
        format!("json_extract({document},'$.{key}') IN ({levels})")
    };
    let app_permission = permission("a.permissions", scope);
    let installation_permission = permission("g.permissions", scope);
    let fine_permission = permission("f.permissions", scope);
    let job_permission = permission("j.permissions", scope);
    let selected = if postgres {
        "(jsonb_array_length(g.repositories::jsonb)=0 OR g.repositories::jsonb @> to_jsonb(r.id))"
    } else {
        "(json_array_length(g.repositories)=0 OR EXISTS(SELECT 1 FROM json_each(g.repositories) choice WHERE choice.value=r.id))"
    };
    let lifetime = if postgres {
        "t.expires_at<=f.requested_at+(COALESCE(p.max_lifetime_days,366)+1)*INTERVAL '1 day'"
    } else {
        "julianday(t.expires_at)<=julianday(f.requested_at)+COALESCE(p.max_lifetime_days,366)+1"
    };
    let app_active = "NOT app_owner.is_banned AND NOT app_owner.must_change_password AND EXISTS(SELECT 1 FROM users app_bot WHERE app_bot.id=a.bot_id AND NOT app_bot.is_banned AND NOT app_bot.must_change_password)";
    let covering = "i.suspended_at IS NULL AND (i.user_id=r.owner_user_id OR i.org_id=r.owner_org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id))";
    let fine_reach = format!("f.status='approved' AND (f.owner_org_id IS NULL OR (EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=f.owner_org_id AND m.user_id=u.id) AND COALESCE(p.fine_grained_allowed,TRUE) AND (p.org_id IS NOT NULL AND p.max_lifetime_days IS NULL OR (t.expires_at IS NOT NULL AND {lifetime})))) AND ((f.owner_org_id IS NULL AND r.owner_user_id=u.id) OR f.owner_org_id=r.owner_org_id) AND (f.repository_access='all' OR (f.repository_access='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories fr WHERE fr.token_id=t.id AND fr.repo_id=r.id)))");
    format!("(({public_read} AND u.id IS NULL AND CAST({token} AS BIGINT) IS NULL AND CAST({session} AS TEXT) IS NULL) OR (u.id IS NOT NULL AND NOT u.is_banned AND NOT u.must_change_password AND (EXISTS(SELECT 1 FROM sessions s WHERE s.user_id=u.id AND s.id={session} AND s.expires_at>{now}) OR EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=CAST({token} AS BIGINT) AND t.user_id=u.id AND t.token_hash={hash} AND {token_scope} AND (t.expires_at IS NULL OR t.expires_at>{now}) AND CASE
        WHEN EXISTS(SELECT 1 FROM github_app_tokens g WHERE g.token_id=t.id) THEN EXISTS(SELECT 1 FROM github_app_tokens g JOIN github_app_installations i ON i.id=g.installation_id JOIN github_apps a ON a.id=i.app_id JOIN users app_owner ON app_owner.id=a.owner_id WHERE g.token_id=t.id AND {app_active} AND {covering} AND {selected} AND {installation_permission})
        WHEN EXISTS(SELECT 1 FROM oauth_app_tokens o JOIN github_apps a ON a.client_id=o.client_id WHERE o.token_id=t.id) THEN EXISTS(SELECT 1 FROM oauth_app_tokens o JOIN github_apps a ON a.client_id=o.client_id JOIN users app_owner ON app_owner.id=a.owner_id JOIN github_app_installations i ON i.app_id=a.id WHERE o.token_id=t.id AND {app_active} AND {covering} AND (i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=u.id) OR EXISTS(SELECT 1 FROM repo_access ra JOIN repositories candidate ON candidate.id=ra.repo_id WHERE ra.user_id=u.id AND ra.access_level BETWEEN 1 AND 5 AND (candidate.owner_user_id=i.user_id OR candidate.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=candidate.id)))) AND {app_permission})
        WHEN EXISTS(SELECT 1 FROM fine_grained_tokens f WHERE f.token_id=t.id) THEN EXISTS(SELECT 1 FROM fine_grained_tokens f LEFT JOIN organization_token_policies p ON p.org_id=f.owner_org_id WHERE f.token_id=t.id AND CASE WHEN {fine_reach} THEN COALESCE({fine_permission},FALSE) ELSE {public} END)
        WHEN EXISTS(SELECT 1 FROM actions_github_tokens j WHERE j.token_id=t.id) THEN EXISTS(SELECT 1 FROM actions_github_tokens j JOIN ci_runs run ON run.id=j.run_id WHERE j.token_id=t.id AND run.repo_id=r.id AND (j.revoked_at IS NULL OR j.revoked_at>{epoch}-60) AND (run.status='running' OR run.completed_at>{recent}) AND {job_permission})
        ELSE (EXISTS(SELECT 1 FROM oauth_app_tokens o WHERE o.token_id=t.id) OR NOT EXISTS(SELECT 1 FROM organization_token_policies p WHERE p.org_id=r.owner_org_id AND NOT p.classic_allowed)) END))))")
}
