use codeyard_db_identity::interactions::InteractionScope;
use codeyard_db_repos::pull_caps::PullCapScope;
use codeyard_webui::components::interactions::InteractionPanel;
use codeyard_webui::components::interactions::InteractionRestriction;
use codeyard_webui::components::interactions::PullCapForm;
pub async fn panel(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    scope: InteractionScope,
    csrf: &str,
    can_set_restriction: bool,
) -> anyhow::Result<InteractionPanel> {
    let (kind, id) = match scope {
        InteractionScope::User(id) => ("user", id),
        InteractionScope::Organization(id) => ("organization", id),
        InteractionScope::Repository(id) => ("repository", id),
    };
    let restriction = codeyard_db_identity::interactions::get_restriction(pool, actor, scope)
        .await?
        .map(|restriction| InteractionRestriction {
            limit: restriction.limit,
            origin: restriction.origin,
            expires_at: restriction.expires_at.to_rfc3339(),
        });
    let cap_scope = match scope {
        InteractionScope::Organization(id) => Some(PullCapScope::Organization(id)),
        InteractionScope::Repository(id) => Some(PullCapScope::Repository(id)),
        InteractionScope::User(_) => None,
    };
    let cap = if let Some(scope) = cap_scope {
        let cap = codeyard_db_repos::pull_caps::get_cap(pool, actor, scope).await?;
        Some(PullCapForm {
            enabled: cap.enabled,
            maximum: cap.max_open_pull_requests.to_string(),
            include_drafts: cap.include_drafts,
        })
    } else {
        None
    };
    let bypass = if let InteractionScope::Repository(repo) = scope {
        codeyard_db_repos::pull_caps::bypass_users(pool, actor, repo)
            .await?
            .into_iter()
            .map(|user| user.username)
            .collect()
    } else {
        Vec::new()
    };
    Ok(InteractionPanel {
        scope: kind.into(),
        scope_id: id,
        csrf_token: csrf.into(),
        restriction,
        can_set_restriction,
        cap,
        bypass,
    })
}
